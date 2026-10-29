#include "user/internal.h"

#include "base/alloc.h"

static void fail(struct user_packet *p, const char *cmd, const char *code, const char *sub,
		 const char *value, const char *text) {
	struct irc_msg *m = IRC_MSG("FAIL", cmd, code, sub);
	if (value != nullptr) {
		irc_msg_add_param(m, value);
	}
	irc_msg_add_param(m, text);
	user_packet_add_take(p, m);
}

void user_history(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		  struct user_packet *p) {
	if (dc->queued + dc->history_pending > 10 * MAX_CHAT_HISTORY_LIMIT) {
		fail(p, "CHATHISTORY", "TEMPORARILY_UNAVAILABLE", "*", nullptr,
		     "Too many messages waiting to be sent, try again later");
		return;
	}
	const char *sub = irc_msg_param(msg, 0), *target = irc_msg_param(msg, 1);
	bool targets = cstr_eq(sub, "TARGETS"), between = cstr_eq(sub, "BETWEEN"),
	     around = cstr_eq(sub, "AROUND");
	if (msg->params.len == 0) {
		user_packet_add_take(
			p, IRC_MSG("461", dc->nick, "CHATHISTORY", "Not enough parameters"));
		return;
	}
	if (!targets && !between && !around && !cstr_eq(sub, "BEFORE") && !cstr_eq(sub, "AFTER") &&
	    !cstr_eq(sub, "LATEST")) {
		fail(p, "CHATHISTORY", "INVALID_PARAMS", sub, nullptr, "Unknown command");
		return;
	}
	if (msg->params.len < (between ? 5U : 4U)) {
		user_packet_add_take(
			p, IRC_MSG("461", dc->nick, "CHATHISTORY", "Not enough parameters"));
		return;
	}
	struct user_network *net = user_get_network(u, dc->network_id);
	if (targets && net == nullptr) {
		p->batch_type = xstrdup("draft/chathistory-targets");
		return;
	}
	if (u->store.db == nullptr) {
		user_packet_add_take(
			p, IRC_MSG("421", dc->nick, "CHATHISTORY", "Chat history disabled"));
		return;
	}
	if (net == nullptr) {
		fail(p, "CHATHISTORY", "INVALID_TARGET", sub, target,
		     "Cannot fetch chat history on bouncer connection");
		return;
	}
	enum casemapping cm =
		net->upstream != nullptr ? net->upstream->casemap : CASEMAPPING_RFC1459;
	AUTOFREE char *canonical = casemap_cstr(cm, target);
	const char *first = irc_msg_param(msg, targets ? 1 : 2);
	const char *second = targets ? irc_msg_param(msg, 2) : between ? irc_msg_param(msg, 3) : "";
	struct db_time a = {0}, b = {0};
	a.set = parse_chathistory_bound(str_from(first), &a.t);
	if (!(cstr_eq(sub, "LATEST") && cstr_eq(first, "*")) && !a.set) {
		fail(p, "CHATHISTORY", "INVALID_PARAMS", sub, first, "Invalid first bound");
		return;
	}
	if (second[0] != '\0') {
		b.set = parse_chathistory_bound(str_from(second), &b.t);
		if (!b.set) {
			fail(p, "CHATHISTORY", "INVALID_PARAMS", sub, second,
			     "Invalid second bound");
			return;
		}
	}
	const char *limit_str = irc_msg_trailing(msg);
	long long limit;
	if (!str_parse_i64(str_from(limit_str), &limit) || limit < 0 ||
	    limit > (long long)MAX_CHAT_HISTORY_LIMIT) {
		fail(p, "CHATHISTORY", "INVALID_PARAMS", sub, limit_str, "Invalid limit");
		return;
	}
	struct server_config *cfg = server_config_get(u->srv);
	size_t count = (size_t)limit;
	if (!targets && count > cfg->cfg.basic.chat_history_limit) {
		count = cfg->cfg.basic.chat_history_limit;
	}
	server_config_unref(cfg);
	struct db_message_options o = {
		.limit = count,
		.events = cap_registry_is_enabled(&dc->caps, "draft/event-playback")};
	struct err *err = nullptr;
	bool ok = true;
	if (cstr_eq(sub, "BEFORE") || around) {
		o.before_time = a;
		o.take_last = true;
	} else if (cstr_eq(sub, "AFTER")) {
		o.after_time = a;
	} else if (cstr_eq(sub, "LATEST")) {
		o.after_time = a;
		o.take_last = true;
	} else {
		bool forward = timestamp_cmp(a.t, b.t) < 0;
		o.after_time = forward ? a : b;
		o.before_time = forward ? b : a;
		o.take_last = !forward;
	}
	if (targets) {
		db_target_last_vec list = {0};
		ok = msgstore_targets(&u->store, net->record.id, &o, &list, &err);
		p->batch_type = xstrdup("draft/chathistory-targets");
		for (size_t i = 0; i < list.len; i++) {
			struct db_target_last *t = &list.data[i];
			struct db_channel *ch = cmmap_get(&net->channels, t->name);
			if (!t->latest_message.set || (ch != nullptr && ch->detached)) {
				continue;
			}
			AUTOFREE char *time = format_server_time_cstr(t->latest_message.t);
			user_packet_add_take(p, IRC_MSG("CHATHISTORY", "TARGETS", t->name, time));
		}
		db_target_last_vec_free(&list);
	} else {
		p->batch_type = xstrdup("chathistory");
		strvec_push(&p->batch_params, target);
		if (around) {
			o.limit = count - count / 2;
		}
		if (!cstr_eq(canonical, SERVICE_NICK_CM)) {
			ok = msgstore_query(&u->store, net->record.id, canonical, &o, &p->messages,
					    &err);
		}
		if (ok && around && !cstr_eq(canonical, SERVICE_NICK_CM)) {
			struct timestamp bound = a.t;
			if (p->messages.len > 0) {
				bool parsed = parse_server_time(
					str_from(irc_tags_get_or_empty(
						&vec_last(&p->messages)->tags, "time")),
					&bound);
				(void)parsed;
			} else {
				bound.sec--;
			}
			o = (struct db_message_options){.after_time = {.set = true, .t = bound},
							.limit = count / 2,
							.events = o.events};
			ok = msgstore_query(&u->store, net->record.id, canonical, &o, &p->messages,
					    &err);
		}
	}
	if (!ok) {
		irc_msg_vec_free(&p->messages);
		xfree(p->batch_type);
		p->batch_type = nullptr;
		fail(p, "CHATHISTORY", "MESSAGE_ERROR", sub, target, "Failed to retrieve messages");
		log_info(&u->srv->log, "failed fetching history: %s", err_msg(err));
	}
	if (ok && p->messages.len > 0) {
		p->history_count = p->messages.len;
		p->history_user = u->record.id;
		dc->history_pending += p->history_count;
	}
	err_free(err);
}

struct read_job {
	int64_t network;
	char *target;
	struct db_read_receipt record;
	bool found, store;
	struct err *err;
};
static void read_job(struct db *db, void *data) {
	struct read_job *j = data;
	bool ok = j->store ? db_store_read_receipt(db, j->network, &j->record, &j->err)
			   : db_get_read_receipt(db, j->network, j->target, &j->record, &j->found,
						 &j->err);
	(void)ok;
}

void user_read_marker(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		      struct user_packet *p) {
	const char *target = irc_msg_param(msg, 0), *criteria = irc_msg_param(msg, 1);
	if (target[0] == '\0' || target[0] == ':' || strchr(target, ' ') != nullptr) {
		fail(p, msg->command, "INVALID_TARGET", "*", nullptr, "Invalid target");
		return;
	}
	struct user_network *net = user_get_network(u, dc->network_id);
	if (net == nullptr) {
		fail(p, msg->command, "INTERNAL_ERROR", target, nullptr,
		     "Cannot set read markers on bouncer connection");
		return;
	}
	enum casemapping cm =
		net->upstream != nullptr ? net->upstream->casemap : CASEMAPPING_RFC1459;
	AUTOFREE char *canonical = casemap_cstr(cm, target);
	struct read_job j = {.network = net->record.id, .target = xstrdup(canonical)};
	db_thread_call(u->srv->db, read_job, &j);
	if (!j.found) {
		db_read_receipt_init(&j.record);
		xfree(j.record.target);
		j.record.target = xstrdup(canonical);
	}
	bool broadcast = false;
	if (j.err == nullptr && criteria[0] != '\0') {
		struct timestamp time;
		if (!parse_chathistory_bound(str_from(criteria), &time)) {
			fail(p, msg->command, "INVALID_PARAMS", criteria, nullptr,
			     "Invalid criteria");
			goto out;
		}
		struct timestamp max = timestamp_now();
		struct timestamp *latest = cmmap_get(&net->latest_times, target);
		if (latest != nullptr && timestamp_cmp(*latest, max) > 0) {
			max = *latest;
		}
		if (latest == nullptr && u->store.db != nullptr) {
			irc_msg_vec history = {0};
			struct db_message_options o = {
				.limit = 1, .take_last = true, .events = true};
			if (msgstore_query(&u->store, net->record.id, canonical, &o, &history,
					   &j.err) &&
			    history.len > 0) {
				struct timestamp t;
				if (parse_server_time(str_from(irc_tags_get_or_empty(
							      &history.data[0]->tags, "time")),
						      &t) &&
				    timestamp_cmp(t, max) > 0) {
					max = t;
				}
			}
			irc_msg_vec_free(&history);
		}
		if (timestamp_cmp(time, max) > 0 &&
		    (!j.record.timestamp.set || timestamp_cmp(time, j.record.timestamp.t) > 0)) {
			time = max;
		}
		if (!j.record.timestamp.set || timestamp_cmp(time, j.record.timestamp.t) > 0) {
			j.record.timestamp = (struct db_time){.set = true, .t = time};
			j.store = true;
			db_thread_call(u->srv->db, read_job, &j);
			broadcast = true;
		}
	}
	if (j.err != nullptr) {
		fail(p, msg->command, "INTERNAL_ERROR", target, nullptr, "Internal error");
		goto out;
	}
	{
		AUTOFREE char *time = j.record.timestamp.set
					      ? format_server_time_cstr(j.record.timestamp.t)
					      : nullptr;
		AUTOFREE char *bound =
			time != nullptr ? xasprintf("timestamp=%s", time) : xstrdup("*");
		struct irc_msg *reply = IRC_MSG(msg->command, target, bound);
		irc_msg_set_prefix(reply, dc->nick, dc->username, dc->hostname);
		user_packet_add_take(p, reply);
		if (broadcast) {
			for (size_t i = 0; i < u->clients.len; i++) {
				struct user_client *other = u->clients.data[i];
				if (other->id == dc->id || other->network_id != net->record.id) {
					continue;
				}
				const char *cmd =
					cap_registry_is_enabled(&other->caps, "draft/read-marker")
						? "MARKREAD"
						: "READ";
				struct irc_msg *copy = IRC_MSG(cmd, target, bound);
				irc_msg_set_prefix(copy, other->nick, other->username,
						   other->hostname);
				user_send_take(u, other, copy);
			}
		}
	}
out:
	db_read_receipt_free(&j.record);
	xfree(j.target);
	err_free(j.err);
}
