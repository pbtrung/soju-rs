#include "upstream/internal.h"

#include "base/alloc.h"

static void send_head(struct upstream *uc, pending_command_vec *q) {
	if (q->len > 0 && q->data[0].sent_at == 0) {
		q->data[0].sent_at = uc->now != 0 ? uc->now : 1;
		upstream_send_labeled_take(uc, q->data[0].downstream_id, q->data[0].label,
					   irc_msg_clone(q->data[0].msg));
	}
}

static void pop(struct upstream *uc, pending_command_vec *q) {
	if (q->len == 0) {
		return;
	}
	xfree(q->data[0].label);
	irc_msg_free(q->data[0].msg);
	vec_remove(q, 0);
	send_head(uc, q);
}

void upstream_abort_take(struct upstream *uc, struct pending_command *p) {
	const char *cmd = p->msg->command;
	struct irc_msg *reply;
	if (cstr_eq(cmd, "LIST")) {
		reply = IRC_MSG("323", uc->nick, "Command aborted");
	} else if (cstr_eq(cmd, "WHO")) {
		reply = IRC_MSG("315", uc->nick, irc_msg_param(p->msg, 0), "Command aborted");
	} else if (cstr_eq(cmd, "WHOIS")) {
		reply = IRC_MSG("318", uc->nick, irc_msg_trailing(p->msg), "Command aborted");
	} else if (cstr_eq(cmd, "AUTHENTICATE")) {
		reply = IRC_MSG("906", uc->nick, "SASL authentication aborted");
	} else {
		reply = IRC_MSG("FAIL", cmd, "TEMPORARILY_UNAVAILABLE", irc_msg_param(p->msg, 0),
				"Command aborted");
	}
	if (p->label[0] != '\0') {
		irc_tags_set(&reply->tags, "label", p->label);
	}
	server_set_prefix(uc->srv, reply);
	upstream_emit_take(uc, reply, p->downstream_id);
	xfree(p->label);
	irc_msg_free(p->msg);
	*p = (struct pending_command){0};
}

void upstream_abort_commands(struct upstream *uc) {
	void *v;
	for (size_t it = 0; strmap_next(&uc->pending, &it, nullptr, &v);) {
		pending_command_vec *q = v;
		for (size_t i = 0; i < q->len; i++) {
			upstream_abort_take(uc, &q->data[i]);
		}
		vec_clear(q);
	}
	strmap_clear(&uc->pending);
}

bool upstream_enqueue_take(struct upstream *uc, uint64_t dc_id, const char *label,
			   struct irc_msg *msg) {
	pending_command_vec *q = strmap_get(&uc->pending, msg->command);
	if (q == nullptr) {
		q = xcalloc(1, sizeof(*q));
		strmap_set(&uc->pending, msg->command, q);
	}
	while (q->len > 0 && q->data[0].sent_at != 0 && uc->now > q->data[0].sent_at &&
	       uc->now - q->data[0].sent_at > 30000) {
		log_info(&uc->log, "timed out waiting for reply to pending command: %s",
			 q->data[0].msg->command);
		pop(uc, q);
	}
	struct pending_command p = {.downstream_id = dc_id, .label = xstrdup(label), .msg = msg};
	if (q->len >= MAX_PENDING_COMMANDS) {
		upstream_abort_take(uc, &p);
		return false;
	}
	bool idle = q->len == 0;
	vec_push(q, p);
	if (idle && uc->ended_command != nullptr && cstr_eq(uc->ended_command, msg->command)) {
		xfree(uc->ended_command);
		xfree(uc->ended_target);
		uc->ended_command = uc->ended_target = nullptr;
	}
	send_head(uc, q);
	return true;
}

void upstream_cancel_downstream(struct upstream *uc, uint64_t dc_id) {
	void *v;
	for (size_t it = 0; strmap_next(&uc->pending, &it, nullptr, &v);) {
		pending_command_vec *q = v;
		for (size_t i = q->len; i > 1; i--) {
			if (q->data[i - 1].downstream_id == dc_id) {
				xfree(q->data[i - 1].label);
				irc_msg_free(q->data[i - 1].msg);
				vec_remove(q, i - 1);
			}
		}
	}
}

void upstream_route_reply(struct upstream *uc, struct irc_msg *msg, uint64_t *dc_id) {
	const char *cmd = nullptr;
	bool end = false, early = false;
	const char *code = msg->command;
	if (cstr_eq(code, "315") || cstr_eq(code, "352") || cstr_eq(code, "354")) {
		cmd = "WHO";
		end = cstr_eq(code, "315");
	} else if (cstr_eq(code, "318") || cstr_eq(code, "401") || cstr_eq(code, "311") ||
		   cstr_eq(code, "312") || cstr_eq(code, "313") || cstr_eq(code, "317") ||
		   cstr_eq(code, "319") || cstr_eq(code, "330") || cstr_eq(code, "671") ||
		   cstr_eq(code, "301")) {
		cmd = "WHOIS";
		end = cstr_eq(code, "318");
		early = cstr_eq(code, "401");
	} else if (cstr_eq(code, "321") || cstr_eq(code, "322") || cstr_eq(code, "323")) {
		cmd = "LIST";
		end = cstr_eq(code, "323");
	} else if (cstr_eq(code, "263")) {
		cmd = irc_msg_param(msg, 1);
		early = true;
	} else if (cstr_eq(code, "903") || cstr_eq(code, "904") || cstr_eq(code, "905") ||
		   cstr_eq(code, "906") || cstr_eq(code, "907")) {
		cmd = "AUTHENTICATE";
		end = true;
	} else if (cstr_eq(code, "REGISTER") || cstr_eq(code, "VERIFY")) {
		cmd = code;
		end = true;
	} else if (cstr_eq(code, "FAIL") && (cstr_eq(irc_msg_param(msg, 0), "REGISTER") ||
					     cstr_eq(irc_msg_param(msg, 0), "VERIFY"))) {
		cmd = irc_msg_param(msg, 0);
		end = true;
	}
	if (cmd == nullptr) {
		return;
	}
	if (end && uc->ended_command != nullptr && cstr_eq(cmd, uc->ended_command)) {
		AUTOFREE char *target = casemap_cstr(uc->casemap, irc_msg_param(msg, 1));
		if (cstr_eq(target, uc->ended_target)) {
			*dc_id = uc->ended_downstream;
			xfree(uc->ended_command);
			xfree(uc->ended_target);
			uc->ended_command = uc->ended_target = nullptr;
			return;
		}
	}
	pending_command_vec *q = strmap_get(&uc->pending, cmd);
	if (q == nullptr || q->len == 0) {
		return;
	}
	struct pending_command *p = &q->data[0];
	if (*dc_id == 0) {
		*dc_id = p->downstream_id;
		if (p->label[0] != '\0' && !irc_tags_contains(&msg->tags, "batch")) {
			irc_tags_set(&msg->tags, "label", p->label);
		}
	}
	if (*dc_id != p->downstream_id) {
		return;
	}
	if (!end && !early && uc->ended_command != nullptr && cstr_eq(uc->ended_command, cmd)) {
		xfree(uc->ended_command);
		xfree(uc->ended_target);
		uc->ended_command = uc->ended_target = nullptr;
	}
	if (early) {
		xfree(uc->ended_command);
		xfree(uc->ended_target);
		uc->ended_command = xstrdup(cmd);
		const char *target =
			cstr_eq(cmd, "WHOIS") ? irc_msg_trailing(p->msg) : irc_msg_param(p->msg, 0);
		uc->ended_target = casemap_cstr(uc->casemap, target);
		uc->ended_downstream = p->downstream_id;
	}
	if (cstr_eq(cmd, "REGISTER") && cstr_eq(code, "REGISTER") && p->msg->params.len >= 3) {
		// Saved with the account on success (soju-rs auto_save_sasl_plain)
		xfree(uc->register_password);
		uc->register_password = xstrdup(irc_msg_param(p->msg, 2));
	}
	if (early || end) {
		pop(uc, q);
	}
}
