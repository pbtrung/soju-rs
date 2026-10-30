#include "user/internal.h"

#include "base/alloc.h"
#include "base/utf8.h"
#include "irc/genmsg.h"

static void copy_caps(struct cap_registry *out, const struct cap_registry *in) {
	const char *name;
	void *value;
	for (size_t it = 0; strmap_next(&in->available, &it, &name, &value);) {
		cap_registry_set_available(out, name, value);
	}
	for (size_t it = 0; strmap_next(&in->enabled, &it, &name, nullptr);) {
		cap_registry_set_enabled(out, name, true);
	}
}

struct user_packet *user_packet_new(const struct user_client *dc, const struct irc_msg *command) {
	struct user_packet *p = xcalloc(1, sizeof(*p));
	p->srv = dc->srv;
	atomic_fetch_add(&p->srv->pending_work, 1);
	p->id = dc->id;
	p->loop = dc->loop;
	p->command = command != nullptr ? irc_msg_clone(command) : nullptr;
	cap_registry_init(&p->caps);
	isupport_init(&p->isupport);
	return p;
}

struct released_history {
	uint64_t id;
	size_t count, queued;
};
static void released_history(struct user *u, void *data) {
	struct released_history *ev = data;
	struct user_client *dc = user_get_client(u, ev->id);
	if (dc != nullptr) {
		dc->history_pending -=
			ev->count < dc->history_pending ? ev->count : dc->history_pending;
		dc->queued = ev->queued;
	}
	xfree(ev);
}

void user_packet_free(void *data) {
	struct user_packet *p = data;
	irc_msg_free(p->command);
	irc_msg_vec_free(&p->messages);
	irc_msg_vec_free(&p->network_messages);
	for (size_t i = 0; i < p->target_metadata.len; i++) {
		irc_msg_vec_free(&p->target_metadata.data[i]);
	}
	vec_free(&p->target_metadata);
	xfree(p->batch_type);
	strvec_free(&p->batch_params);
	xfree(p->label);
	xfree(p->batch_tag);
	for (size_t i = 0; i < sizeof(p->state_strings) / sizeof(*p->state_strings); i++) {
		xfree(p->state_strings[i]);
	}
	if (p->history_count != 0) {
		struct released_history *ev = xmalloc(sizeof(*ev));
		*ev = (struct released_history){
			.id = p->id, .count = p->history_count, .queued = p->queued};
		user_post(p->srv, p->history_user, released_history, xfree, ev);
	}
	atomic_fetch_sub(&p->srv->pending_work, 1);
	cap_registry_free(&p->caps);
	isupport_free(&p->isupport);
	xfree(p);
}

void user_packet_add_take(struct user_packet *p, struct irc_msg *msg) {
	vec_push(&p->messages, msg);
}

static void deliver(void *data) {
	struct user_packet *p = data;
	struct downstream *dc = downstream_lookup(p->id);
	if (dc != nullptr) {
		if (p->attach) {
			struct downstream *child = downstream_attach(dc, p->network_id);
			user_downstream_registered(child, nullptr);
		} else if (p->detach) {
			struct downstream *child = downstream_find_attached(dc->id, p->network_id);
			if (child != nullptr) {
				downstream_detach(child);
			}
		}
		if (p->command != nullptr) {
			downstream_begin_command(dc, p->command);
		}
		dc->forced_batch = p->batch_tag;
		if (p->welcome) {
			downstream_welcome(dc, &p->state);
		} else if (p->sync) {
			downstream_sync(dc, &p->state);
		}
		if (p->send_networks) {
			downstream_send_batch(dc, "soju.im/bouncer-networks", nullptr, 0,
					      &p->network_messages);
		}
		if (p->welcome && cap_registry_is_enabled(&dc->caps, "draft/metadata-2")) {
			irc_msg_vec empty = {0};
			downstream_send_batch(dc, "metadata", nullptr, 0, &empty);
			for (size_t i = 0; i < p->target_metadata.len; i++) {
				downstream_send_batch(dc, "metadata", nullptr, 0,
						      &p->target_metadata.data[i]);
			}
		}
		if (p->metadata_updates_unbatched) {
			AUTOFREE char *batch =
				downstream_begin_batch(dc, "metadata", nullptr, 0, nullptr);
			for (size_t i = 0; i < p->messages.len; i++) {
				struct irc_msg *msg = p->messages.data[i];
				if (!cstr_eq(msg->command, "METADATA")) {
					irc_tags_set(&msg->tags, "batch", batch);
				}
				downstream_send(dc, msg);
			}
			vec_clear(&p->messages);
			downstream_end_batch(dc, batch);
		} else if (p->batch_type != nullptr) {
			downstream_send_batch(dc, p->batch_type,
					      (const char *const *)p->batch_params.data,
					      p->batch_params.len, &p->messages);
		} else {
			for (size_t i = 0; i < p->messages.len; i++) {
				downstream_send(dc, p->messages.data[i]);
			}
			vec_clear(&p->messages);
		}
		dc->forced_batch = nullptr;
		p->queued = conn_queued(dc->conn);
		if (p->command != nullptr) {
			downstream_end_command(dc);
		}
		if (p->close) {
			downstream_close(dc);
		}
		if (p->welcome || p->sync) {
			user_downstream_updated(dc, nullptr);
			if (p->welcome && !dc->has_parent) {
				downstream_resume_input(dc);
			}
		}
	}
	if (p->ack) {
		struct downstream *source = downstream_lookup(p->ack_id);
		if (source != nullptr) {
			downstream_input_done(source);
		}
	}
	user_packet_free(p);
}

void user_packet_send_take(struct user_packet *p) {
	loop_post(p->loop, deliver, user_packet_free, p);
}

void user_send_take(struct user *u, const struct user_client *dc, struct irc_msg *msg) {
	(void)u;
	struct user_packet *p = user_packet_new(dc, nullptr);
	user_packet_add_take(p, msg);
	user_packet_send_take(p);
}

void user_service_reply(struct user *u, uint64_t dc_id, const char *text) {
	struct user_client *dc = user_get_client(u, dc_id);
	if (dc == nullptr) {
		return;
	}
	struct irc_msg *msg = IRC_MSG("PRIVMSG", dc->nick, text);
	irc_msg_set_prefix(msg, SERVICE_NICK, SERVICE_NICK, SERVICE_NICK);
	user_send_take(u, dc, msg);
}

void user_packet_sync(struct user *u, struct user_network *net, struct user_packet *p,
		      bool welcome) {
	struct upstream *uc = net != nullptr && net->upstream != nullptr && net->upstream->attached
				      ? net->upstream
				      : nullptr;
	const struct db_network *record = net != nullptr ? &net->record : nullptr;
	const char *nick = uc != nullptr ? uc->nick : db_get_nick(&u->record, record);
	const char *username = uc != nullptr ? uc->username : db_get_username(&u->record, record);
	const char *host = uc != nullptr ? uc->hostname : "";
	const char *realname = uc != nullptr ? uc->realname : db_get_realname(&u->record, record);
	const char *account = uc != nullptr    ? uc->account
			      : net == nullptr ? u->record.username
					       : "";
	const char *modes = uc != nullptr ? strbuf_cstr(&uc->modes.s) : "";
	const char *values[] = {nick, username, host, realname, account, modes};
	for (size_t i = 0; i < sizeof(values) / sizeof(*values); i++) {
		p->state_strings[i] = xstrdup(values[i]);
	}
	p->state = (struct downstream_sync){.nick = p->state_strings[0],
					    .username = p->state_strings[1],
					    .hostname = p->state_strings[2],
					    .realname = p->state_strings[3],
					    .account = p->state_strings[4],
					    .modes = p->state_strings[5],
					    .admin = u->record.admin,
					    .has_network = net != nullptr,
					    .network_id = net != nullptr ? net->record.id : 0,
					    .chathistory = u->store.db != nullptr,
					    .casemap = uc != nullptr    ? uc->casemap
						       : net != nullptr ? CASEMAPPING_RFC1459
									: CASEMAPPING_ASCII};
	if (uc != nullptr) {
		copy_caps(&p->caps, &uc->caps);
		const char *key;
		void *v;
		for (size_t it = 0; strmap_next(&uc->isupport.m, &it, &key, &v);) {
			isupport_set(&p->isupport, key, v);
		}
		p->state.up_caps = &p->caps;
		p->state.up_isupport = &p->isupport;
	}
	// soju-rs updates ISUPPORT on each RPL_ISUPPORT of a registered
	// connection, not when it disconnects
	p->state.keep_isupport =
		!welcome && net != nullptr && (uc == nullptr || strmap_len(&uc->isupport.m) == 0);
	p->state.account_first = !welcome;
	p->welcome = welcome;
	p->sync = !welcome;
}

struct read_receipt_job {
	int64_t network;
	const char *target;
	struct db_read_receipt record;
	bool found;
	struct err *err;
};
static void get_read_receipt(struct db *db, void *data) {
	struct read_receipt_job *j = data;
	bool ok = db_get_read_receipt(db, j->network, j->target, &j->record, &j->found, &j->err);
	(void)ok;
}

// soju-rs downstream::forward_channel: a complete channel's state, after
// its JOIN
void user_forward_channel(struct user *u, struct user_network *net, const struct user_client *dc,
			  struct user_packet *p, const struct upstream_channel *ch) {
	struct upstream *uc = net->upstream;
	// RPL_NOTOPIC isn't sent on JOIN
	if (ch->topic[0] != '\0') {
		user_packet_add_take(p, IRC_MSG("332", dc->nick, ch->name, ch->topic));
		if (ch->topic_who[0] != '\0') {
			AUTOFREE char *time = xasprintf(
				"%lld", (long long)(ch->topic_time.set ? ch->topic_time.t.sec
								       : -62135596800LL));
			user_packet_add_take(
				p, IRC_MSG("333", dc->nick, ch->name, ch->topic_who, time));
		}
	}
	const char *read_cmd = cap_registry_is_enabled(&dc->caps, "draft/read-marker") ? "MARKREAD"
			       : cap_registry_is_enabled(&dc->caps, "soju.im/read")    ? "READ"
										       : nullptr;
	if (read_cmd != nullptr) {
		AUTOFREE char *target = casemap_cstr(uc->casemap, ch->name);
		struct read_receipt_job j = {.network = net->record.id, .target = target};
		db_thread_call(u->srv->db, get_read_receipt, &j);
		if (j.err != nullptr) {
			AUTOFREE char *q = utf8_debug_cstr(str_from(ch->name));
			log_info(&u->srv->log, "failed to get the read receipt for %s: %s", q,
				 err_msg(j.err));
			err_free(j.err);
		} else {
			AUTOFREE char *time =
				j.found && j.record.timestamp.set
					? format_server_time_cstr(j.record.timestamp.t)
					: nullptr;
			AUTOFREE char *ts =
				time != nullptr ? xasprintf("timestamp=%s", time) : xstrdup("*");
			struct irc_msg *m = IRC_MSG(read_cmd, ch->name, ts);
			irc_msg_set_prefix(m, DOWNSTREAM_SELF_PREFIX, "", "");
			user_packet_add_take(p, m);
		}
		if (j.found) {
			db_read_receipt_free(&j.record);
		}
	}
	if (!cap_registry_is_enabled(&dc->caps, "no-implicit-names") &&
	    !cap_registry_is_enabled(&dc->caps, "draft/no-implicit-names") &&
	    !cap_registry_is_enabled(&dc->caps, "soju.im/no-implicit-names")) {
		user_names_reply(dc, p, ch);
	}
}

void user_names_reply(const struct user_client *dc, struct user_packet *p,
		      const struct upstream_channel *ch) {
	strvec members = {0};
	const char *name;
	void *v;
	for (size_t it = 0; cmmap_next(&ch->members, &it, &name, &v);) {
		AUTO_STRBUF struct strbuf member = STRBUF_INIT;
		format_member_prefix(&member, v,
				     cap_registry_is_enabled(&dc->caps, "multi-prefix"));
		strbuf_append_cstr(&member, name);
		strvec_push(&members, strbuf_cstr(&member));
	}
	generate_names_reply(&p->messages, ch->name, ch->status, (const char *const *)members.data,
			     members.len);
	strvec_free(&members);
}

void user_channel_replay(struct user *u, struct user_network *net, const struct user_client *dc,
			 struct user_packet *p, const struct upstream_channel *ch, bool extended) {
	struct upstream *uc = net->upstream;
	struct irc_msg *join = IRC_MSG("JOIN", ch->name);
	irc_msg_set_prefix(join, DOWNSTREAM_SELF_PREFIX, "", "");
	if (extended && cap_registry_is_enabled(&dc->caps, "extended-join")) {
		irc_msg_add_param(join, uc->account[0] != '\0' ? uc->account : "*");
		irc_msg_add_param(join, uc->realname);
	}
	user_packet_add_take(p, join);
	user_forward_channel(u, net, dc, p, ch);
}
