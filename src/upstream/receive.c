// Messages from the upstream server: batches, registration errors, then
// the state handlers and the routing of replies to downstreams (soju-rs
// upstream/mod.rs handle_message)
#include "upstream/internal.h"

#include "base/alloc.h"
#include "base/utf8.h"
#include "irc/numerics.h"

// BATCH +: returns true if the message is consumed (a multiline batch, a
// batch which isn't a labeled response, or a batch we refuse)
static bool open_batch(struct upstream *uc, struct irc_msg *msg, const char *label,
		       const struct upstream_batch *parent, uint64_t dc_id) {
	const char *ref = irc_msg_param(msg, 0);
	if (uc->batches.live >= 64 || strmap_has(&uc->batches, ref + 1)) {
		return true;
	}
	struct upstream_batch *b = xcalloc(1, sizeof(*b));
	b->downstream_id = dc_id;
	b->label = xstrdup(label[0] != '\0' ? label : parent != nullptr ? parent->label : "");
	if (cstr_eq(irc_msg_param(msg, 1), MULTILINE_BATCH_TYPE)) {
		b->multiline = xcalloc(1, sizeof(*b->multiline));
		multiline_batch_init(b->multiline, irc_msg_clone(msg));
	}
	strmap_set(&uc->batches, ref + 1, b);
	return b->multiline != nullptr || b->label[0] == '\0';
}

// BATCH -: returns true if the message is consumed. A multiline batch is
// handled as its logical message.
static bool close_batch(struct upstream *uc, struct irc_msg *msg, uint64_t *dc_id) {
	struct upstream_batch *b = strmap_take(&uc->batches, irc_msg_param(msg, 0) + 1);
	if (b == nullptr) {
		return true;
	}
	*dc_id = b->downstream_id;
	if (b->multiline != nullptr) {
		enum multiline_error error;
		struct irc_msg *logical = multiline_batch_finish(b->multiline, &error);
		xfree(b->multiline);
		b->multiline = nullptr;
		if (logical != nullptr) {
			irc_tags_remove(&logical->tags, "label");
			if (b->downstream_id != 0) {
				AUTOFREE char *original =
					xasprintf("sd-%llu-0-%s",
						  (unsigned long long)b->downstream_id, b->label);
				irc_tags_set(&logical->tags, "label", original);
			}
			upstream_receive_take(uc, logical);
		}
		upstream_free_batch(b);
		return true;
	}
	bool hidden = b->label[0] == '\0';
	upstream_free_batch(b);
	return hidden;
}

// ERROR, and registration failures: returns true if the message is
// consumed
static bool handle_errors(struct upstream *uc, const struct irc_msg *msg) {
	if (cstr_eq(msg->command, "ERROR")) {
		// soju-rs: a handler error, which only aborts registration; a
		// registered connection is then closed by the server
		AUTOFREE char *text = xasprintf("fatal server error: %s", irc_msg_param(msg, 0));
		if (uc->registered) {
			log_info(&uc->log, "failed to handle message: %s", text);
			return true;
		}
		struct irc_msg *bare = irc_msg_clone(msg);
		irc_tags_clear(&bare->tags);
		AUTOFREE char *line = irc_msg_to_string(bare);
		irc_msg_free(bare);
		AUTOFREE char *q = utf8_debug_cstr(str_from(line));
		AUTOFREE char *reason =
			xasprintf("failed to register: failed to handle message %s: %s", q, text);
		upstream_disconnect(uc, reason, true);
		return true;
	}
	if (!uc->registered &&
	    (cstr_eq(msg->command, ERR_PASSWDMISMATCH) ||
	     cstr_eq(msg->command, ERR_ERRONEUSNICKNAME) || cstr_eq(msg->command, "FAIL"))) {
		AUTOFREE char *error = upstream_registration_error(msg);
		AUTOFREE char *reason = xasprintf("failed to register: %s", error);
		upstream_disconnect(uc, reason, upstream_registration_temporary(msg));
		return true;
	}
	if (!uc->registered && cstr_eq(msg->command, ERR_NICKNAMEINUSE)) {
		AUTOFREE char *nick = xasprintf("%s_", uc->nick);
		xfree(uc->nick);
		uc->nick = xstrdup(nick);
		upstream_send_take(uc, IRC_MSG("NICK", nick));
		return true;
	}
	return false;
}

// Returns true if the message is consumed before the handlers: PING, and
// messages delayed until the connection is attached
static bool handle_early(struct upstream *uc, struct irc_msg **msg) {
	if (cstr_eq((*msg)->command, "PING")) {
		upstream_send_take(uc, IRC_MSG("PONG", irc_msg_trailing(*msg)));
		return true;
	}
	if (uc->registered && !uc->attached && !cstr_eq((*msg)->command, "001")) {
		if (uc->delayed.len >= MAX_INPUT_IN_FLIGHT) {
			upstream_disconnect(uc, "too many messages during registration delay",
					    true);
			return true;
		}
		vec_push(&uc->delayed, *msg);
		*msg = nullptr;
		return true;
	}
	return false;
}

bool upstream_parse_label(const char *label, uint64_t *dc_id, char **out, struct err **err) {
	*dc_id = 0;
	*out = nullptr;
	if (label[0] == '\0') {
		*out = xstrdup("");
		return true;
	}
	struct str rest = str_from(label), part;
	unsigned long long n;
	if (!str_next_field(&rest, '-', &part) || !str_eq_cstr(part, "sd") ||
	    !str_next_field(&rest, '-', &part) || !str_parse_u64(part, &n)) {
		return err_set(err, 0, "invalid label");
	}
	*dc_id = n;
	if (!str_next_field(&rest, '-', &part) || rest.p == nullptr) {
		return err_set(err, 0, "invalid label");
	}
	*out = str_dup(rest);
	return true;
}

bool upstream_registration_temporary(const struct irc_msg *msg) {
	return !cstr_eq(msg->command, ERR_PASSWDMISMATCH) &&
	       !cstr_eq(msg->command, ERR_ERRONEUSNICKNAME) &&
	       !(cstr_eq(msg->command, "FAIL") &&
		 cstr_eq(irc_msg_param(msg, 1), "ACCOUNT_REQUIRED"));
}

char *upstream_registration_error(const struct irc_msg *msg) {
	return xasprintf("registration error (%s): %s", msg->command,
			 msg->params.len > 0 ? irc_msg_trailing(msg) : msg->command);
}

void upstream_receive_take(struct upstream *uc, struct irc_msg *msg) {
	AUTOFREE char *label = nullptr;
	if (uc->closing) {
		irc_msg_free(msg);
		return;
	}
	atomic_fetch_add(&uc->srv->metrics.upstream_in_messages_total, 1);
	if (handle_early(uc, &msg)) {
		goto out;
	}
	uint64_t dc_id = 0;
	const char *raw_label = irc_tags_get_or_empty(&msg->tags, "label");
	if (!upstream_parse_label(raw_label, &dc_id, &label, nullptr) || label == nullptr) {
		goto out;
	}
	const char *parent_ref = irc_tags_get(&msg->tags, "batch");
	struct upstream_batch *parent =
		parent_ref != nullptr ? strmap_get(&uc->batches, parent_ref) : nullptr;
	if (parent != nullptr && dc_id == 0) {
		dc_id = parent->downstream_id;
	}
	// soju-rs forwards an upstream batch only as the labeled response of a
	// labeled downstream command: messages of other batches lose their tag
	if (parent != nullptr && parent->label[0] == '\0') {
		irc_tags_remove(&msg->tags, "batch");
	}
	irc_tags_remove(&msg->tags, "label");
	if (label[0] != '\0') {
		irc_tags_set(&msg->tags, "label", label);
	}
	const char *ref = cstr_eq(msg->command, "BATCH") ? irc_msg_param(msg, 0) : "";
	if ((ref[0] == '+' && open_batch(uc, msg, label, parent, dc_id)) ||
	    (ref[0] == '-' && close_batch(uc, msg, &dc_id))) {
		goto out;
	}
	if (!(cstr_eq(msg->command, "BATCH") && msg->params.len > 0) && parent != nullptr &&
	    parent->multiline != nullptr) {
		struct multiline_limits limits = multiline_limits_default();
		multiline_batch_push(parent->multiline, msg, &limits);
		goto out;
	}
	if (cstr_eq(msg->command, "BATCH")) {
		// A labeled response: it comes from us
		server_set_prefix(uc->srv, msg);
	}
	if (parent != nullptr && parent->label[0] != '\0' && dc_id != 0) {
		uc->batch_downstream = dc_id;
		uc->batch_ref = xstrdup(parent_ref);
	}
	if (upstream_handle_registration(uc, msg) || handle_errors(uc, msg) ||
	    !upstream_handle_channel(uc, msg)) {
		goto out;
	}
	upstream_route_reply(uc, msg, &dc_id);
	upstream_emit_take(uc, msg, dc_id);
	msg = nullptr;
out:
	uc->batch_downstream = 0;
	xfree(uc->batch_ref);
	uc->batch_ref = nullptr;
	irc_msg_free(msg);
}
