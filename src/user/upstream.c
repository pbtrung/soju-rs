#include "user/internal.h"

#include "base/alloc.h"
#include "irc/genmsg.h"

static bool same(enum casemapping cm, const char *a, const char *b) {
	AUTOFREE char *ca = casemap_cstr(cm, a);
	AUTOFREE char *cb = casemap_cstr(cm, b);
	return cstr_eq(ca, cb);
}

static char *append(struct user *u, struct user_network *net, const char *target,
		    const struct irc_msg *msg) {
	if (target[0] == '\0' || target[0] == '$') {
		return xstrdup("");
	}
	enum casemapping cm =
		net->upstream != nullptr ? net->upstream->casemap : CASEMAPPING_RFC1459;
	AUTOFREE char *canonical = casemap_cstr(cm, target);
	if (cstr_eq(canonical, "nickserv")) {
		return xstrdup("");
	}
	if (cstr_eq(msg->command, "TAGMSG") && !irc_tags_contains(&msg->tags, "+react") &&
	    !irc_tags_contains(&msg->tags, "+unreact") &&
	    !irc_tags_contains(&msg->tags, "+draft/react") &&
	    !irc_tags_contains(&msg->tags, "+draft/unreact")) {
		return xstrdup("");
	}
	// The first message of a target: the clients with delivery receipts get
	// a baseline, to receive it in their backlog (soju-rs append_log)
	if (cmmap_len(&net->delivered.targets) > 0 && !cmmap_has(&net->delivered.targets, target)) {
		struct strmap clients;
		strmap_init(&clients, nullptr);
		void *v;
		const char *name;
		for (size_t it = 0; cmmap_next(&net->delivered.targets, &it, nullptr, &v);) {
			for (size_t jt = 0; strmap_next(v, &jt, &name, nullptr);) {
				strmap_set(&clients, name, nullptr);
			}
		}
		char *last = nullptr;
		if (msgstore_last_id(&u->store, net->record.id, canonical, &last, nullptr)) {
			for (size_t it = 0; strmap_next(&clients, &it, &name, nullptr);) {
				delivered_set(&net->delivered, target, name, last);
			}
		}
		xfree(last);
		strmap_free(&clients);
	}
	struct err *err = nullptr;
	char *id = nullptr;
	if (!msgstore_append(&u->store, net->record.id, canonical, msg, &id, &err)) {
		log_info(&u->srv->log, "failed to append message: %s", err_msg(err));
		err_free(err);
		return xstrdup("");
	}
	struct timestamp time;
	if (parse_server_time(str_from(irc_tags_get_or_empty(&msg->tags, "time")), &time)) {
		struct timestamp *last = cmmap_get(&net->latest_times, target);
		if (last == nullptr) {
			last = xmalloc(sizeof(*last));
			*last = time;
			cmmap_set(&net->latest_times, target, last);
		} else if (timestamp_cmp(time, *last) > 0) {
			*last = time;
		}
	}
	return id;
}

void user_produce(struct user *u, struct user_network *net, struct irc_msg *msg, const char *target,
		  uint64_t origin_id, uint64_t reply_id) {
	struct timestamp time;
	if (!parse_server_time(str_from(irc_tags_get_or_empty(&msg->tags, "time")), &time)) {
		AUTOFREE char *now = format_server_time_cstr(timestamp_now());
		irc_tags_set(&msg->tags, "time", now);
	}
	AUTOFREE char *id = append(u, net, target, msg);
	struct db_channel *ch = cmmap_get(&net->channels, target);
	bool detached = ch != nullptr && ch->detached;
	// Channel events (JOIN, PART...) are never blocked
	bool event = !cstr_eq(msg->command, "PRIVMSG") && !cstr_eq(msg->command, "NOTICE") &&
		     !cstr_eq(msg->command, "TAGMSG") && !cstr_eq(msg->command, "REDACT");
	bool blocked = !event && cmmap_has(&net->blocked_targets, irc_msg_prefix_name(msg));
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *dc = u->clients.data[i];
		if (dc->network_id != net->record.id ||
		    (blocked && !strmap_has(&dc->metadata_subs, "soju.im/blocked"))) {
			continue;
		}
		bool echo = dc->id == origin_id;
		struct user_packet *p = user_packet_new(dc, nullptr);
		if (!detached && (!echo || cap_registry_is_enabled(&dc->caps, "echo-message"))) {
			struct irc_msg *copy = irc_msg_clone(msg);
			if (dc->id != reply_id) {
				irc_tags_remove(&copy->tags, "label");
			}
			user_packet_add_take(p, copy);
		}
		if (id[0] != '\0' && !cap_registry_is_enabled(&dc->caps, "draft/chathistory") &&
		    (cstr_eq(msg->command, "PRIVMSG") || cstr_eq(msg->command, "NOTICE"))) {
			AUTOFREE char *ping = xasprintf("soju-msgid-%s", id);
			user_packet_add_take(p, IRC_MSG("PING", ping));
		}
		user_packet_send_take(p);
	}
}

// soju-rs handle_detached_message: a message of a detached channel may be
// relayed as a notice, and reattach it
static void handle_detached_message(struct user *u, struct user_network *net,
				    const struct irc_msg *msg, const char *buffer, bool from_us) {
	struct db_channel *ch = buffer[0] != '\0' ? cmmap_get(&net->channels, buffer) : nullptr;
	if (ch == nullptr || !ch->detached || from_us ||
	    (!cstr_eq(msg->command, "PRIVMSG") && !cstr_eq(msg->command, "NOTICE"))) {
		return;
	}
	bool highlight = user_network_highlight(u, net, msg);
	bool relay =
		ch->relay_detached == FILTER_MESSAGE ||
		((ch->relay_detached == FILTER_HIGHLIGHT || ch->relay_detached == FILTER_DEFAULT) &&
		 highlight);
	bool reattach = ch->reattach_on == FILTER_MESSAGE ||
			(ch->reattach_on == FILTER_HIGHLIGHT && highlight);
	if (relay) {
		AUTOFREE char *text = xasprintf(
			"%s in %s: <%s> %s", highlight ? "highlight" : "message",
			irc_msg_param(msg, 0), irc_msg_prefix_name(msg), irc_msg_param(msg, 1));
		for (size_t i = 0; i < u->clients.len; i++) {
			struct user_client *dc = u->clients.data[i];
			if (dc->network_id == net->record.id) {
				struct irc_msg *notice = IRC_MSG("NOTICE", dc->nick, text);
				irc_msg_set_prefix(notice, SERVICE_NICK, SERVICE_NICK,
						   SERVICE_NICK);
				user_send_take(u, dc, notice);
			}
		}
	}
	if (reattach) {
		AUTOFREE char *name = xstrdup(buffer);
		user_attach_channel(u, net, name);
	}
}

// Clients learn which conversations mention them from CHATHISTORY TARGETS
// (soju.im/unread), without loading their history
static void record_mention(struct user *u, struct user_network *net, const struct irc_msg *msg,
			   const char *buffer, bool direct, bool from_us) {
	bool is_text = cstr_eq(msg->command, "PRIVMSG") || cstr_eq(msg->command, "NOTICE");
	if (!is_text || from_us || buffer[0] == '\0' || u->store.db == nullptr ||
	    cmmap_has(&net->blocked_targets, irc_msg_prefix_name(msg))) {
		return;
	}
	// A private PRIVMSG, which clients show in its own conversation,
	// unlike NOTICEs (e.g. from services) and CTCP requests or replies
	const char *text = irc_msg_param(msg, 1);
	bool mention =
		direct ? cstr_eq(msg->command, "PRIVMSG") && (text[0] != '\x01' || strncmp(text,
											   "\x01"
											   "ACTION",
											   7) == 0)
		       : user_network_highlight(u, net, msg);
	if (!mention) {
		return;
	}
	struct timestamp time;
	if (!parse_server_time(str_from(irc_tags_get_or_empty(&msg->tags, "time")), &time)) {
		time = timestamp_now();
	}
	enum casemapping cm =
		net->upstream != nullptr ? net->upstream->casemap : CASEMAPPING_RFC1459;
	AUTOFREE char *target = casemap_cstr(cm, buffer);
	user_record_mention(u, net->record.id, target, time);
}

void user_upstream_message(struct upstream *uc, struct irc_msg *msg, uint64_t dc_id, void *data) {
	struct user *u = user_lookup(uc->srv, (int64_t)(uintptr_t)data);
	struct user_network *net = u != nullptr ? user_get_network(u, uc->network.id) : nullptr;
	if (net == nullptr || net->upstream != uc || u->stopped) {
		goto out;
	}
	const char *source = irc_msg_prefix_name(msg), *cmd = msg->command;
	if (cstr_eq(cmd, "305") || cstr_eq(cmd, "306") ||
	    (cstr_eq(cmd, "AWAY") && same(uc->casemap, source, uc->nick))) {
		goto out;
	}
	if (cstr_eq(cmd, "PRIVMSG") || cstr_eq(cmd, "NOTICE") || cstr_eq(cmd, "TAGMSG") ||
	    cstr_eq(cmd, "REDACT")) {
		const char *target = irc_msg_param(msg, 0);
		while (target[0] != '\0' && strchr(uc->status_msg, target[0]) != nullptr) {
			target++;
		}
		if (same(uc->casemap, source, SERVICE_NICK) ||
		    same(uc->casemap, target, SERVICE_NICK)) {
			goto out;
		}
		bool from_us = same(uc->casemap, source, uc->nick);
		bool direct = same(uc->casemap, target, uc->nick);
		const char *buffer = direct ? source : target;
		if (!uc->registered || same(uc->casemap, source, uc->server_name) ||
		    cstr_eq(target, "*") || target[0] == '$') {
			buffer = "";
		}
		const char *context = irc_tags_get(&msg->tags, "+draft/channel-context");
		struct upstream_channel *channel =
			context != nullptr ? cmmap_get(&uc->channels, context) : nullptr;
		if (channel != nullptr && cmmap_has(&channel->members, source)) {
			buffer = channel->name;
			direct = false;
		}
		handle_detached_message(u, net, msg, buffer, from_us);
		user_produce(u, net, msg, buffer, from_us ? dc_id : 0, dc_id);
		user_web_push_message(u, net, msg, buffer, direct, from_us);
		record_mention(u, net, msg, buffer, direct, from_us);
		goto out;
	}
	if (cstr_eq(cmd, "REGISTER") && uc->register_password != nullptr && msg->params.len >= 2) {
		user_auto_save_sasl_plain(u, net, irc_msg_param(msg, 1), uc->register_password);
		xfree(uc->register_password);
		uc->register_password = nullptr;
	}
	if ((cstr_eq(cmd, "902") || cstr_eq(cmd, "903") || cstr_eq(cmd, "904") ||
	     cstr_eq(cmd, "905") || cstr_eq(cmd, "906") || cstr_eq(cmd, "907")) &&
	    uc->sasl_username != nullptr) {
		if (cstr_eq(cmd, "903")) {
			user_auto_save_sasl_plain(u, net, uc->sasl_username, uc->sasl_password);
		}
		xfree(uc->sasl_username);
		if (uc->sasl_password != nullptr) {
			explicit_bzero(uc->sasl_password, strlen(uc->sasl_password));
		}
		xfree(uc->sasl_password);
		uc->sasl_username = uc->sasl_password = nullptr;
	}
	if (cstr_eq(cmd, "730") || cstr_eq(cmd, "731")) {
		struct str rest = str_from(irc_msg_param(msg, 1)), part;
		while (str_next_field(&rest, ',', &part)) {
			struct str name, username, hostname;
			irc_prefix_split(part, &name, &username, &hostname);
			AUTOFREE char *nick = str_dup(name);
			for (size_t i = 0; i < u->clients.len; i++) {
				struct user_client *dc = u->clients.data[i];
				if (dc->network_id == net->record.id &&
				    cmmap_has(&dc->monitored, nick)) {
					AUTOFREE char *prefix = str_dup(part);
					user_send_take(u, dc, IRC_MSG(cmd, uc->nick, prefix));
				}
			}
		}
		goto out;
	}
	if (cstr_eq(cmd, "JOIN") && same(uc->casemap, source, uc->nick)) {
		const char *channel = irc_msg_param(msg, 0);
		struct db_channel *saved = cmmap_get(&net->channels, channel);
		if (saved == nullptr) {
			struct db_channel record;
			db_channel_init(&record);
			xfree(record.name);
			record.name = xstrdup(channel);
			bool ok = user_store_channel(u, net, &record, nullptr);
			(void)ok;
			db_channel_free(&record);
			saved = cmmap_get(&net->channels, channel);
		}
		if (saved != nullptr) {
			upstream_set_auto_detach(uc, channel, saved->detach_after_ns / 1000000);
		}
	}
	// soju-rs stores channel events with the channel's messages, and the
	// QUIT or NICK of a member with each of its channels
	if (cstr_eq(cmd, "JOIN") || cstr_eq(cmd, "PART") || cstr_eq(cmd, "KICK") ||
	    cstr_eq(cmd, "TOPIC")) {
		if (cmmap_has(&uc->channels, irc_msg_param(msg, 0)) || !cstr_eq(cmd, "TOPIC")) {
			user_produce(u, net, msg, irc_msg_param(msg, 0), 0, dc_id);
			goto out;
		}
	} else if (cstr_eq(cmd, "MODE") && upstream_is_channel(uc, irc_msg_param(msg, 0)) &&
		   cmmap_has(&uc->channels, irc_msg_param(msg, 0))) {
		AUTOFREE char *id = append(u, net, irc_msg_param(msg, 0), msg);
	} else if (cstr_eq(cmd, "QUIT") || cstr_eq(cmd, "NICK")) {
		for (size_t i = 0; i < uc->event_channels.len; i++) {
			AUTOFREE char *id = append(u, net, uc->event_channels.data[i], msg);
		}
		strvec_free(&uc->event_channels);
	}
	if (((cstr_eq(cmd, "NICK") || cstr_eq(cmd, "QUIT")) &&
	     same(uc->casemap, source, uc->nick)) ||
	    (cstr_eq(cmd, "NICK") && same(uc->casemap, irc_msg_param(msg, 0), uc->nick)) ||
	    ((cstr_eq(cmd, "CHGHOST") || cstr_eq(cmd, "SETNAME")) &&
	     same(uc->casemap, source, uc->nick))) {
		goto out;
	}
	const char *channel = nullptr;
	if (cstr_eq(cmd, "TOPIC") || cstr_eq(cmd, "MODE")) {
		channel = irc_msg_param(msg, 0);
	} else if (cstr_eq(cmd, "332") || cstr_eq(cmd, "333") || cstr_eq(cmd, "366") ||
		   cstr_eq(cmd, "324") || cstr_eq(cmd, "329")) {
		channel = irc_msg_param(msg, 1);
	} else if (cstr_eq(cmd, "353")) {
		channel = irc_msg_param(msg, 2);
	}
	if (channel != nullptr) {
		struct db_channel *saved = cmmap_get(&net->channels, channel);
		if (saved != nullptr && saved->detached) {
			goto out;
		}
	}
	// Replies go to the client which sent the command, other messages to
	// all, with the label only for that client (soju-rs forward_msg_by_id
	// and forward_message)
	bool reply = is_numeric(cmd) || cstr_eq(cmd, "FAIL") || cstr_eq(cmd, "WARN") ||
		     cstr_eq(cmd, "NOTE") || cstr_eq(cmd, "BATCH") || cstr_eq(cmd, "ACK") ||
		     cstr_eq(cmd, "REGISTER") || cstr_eq(cmd, "VERIFY");
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *dc = u->clients.data[i];
		if (dc->network_id == net->record.id && (dc_id == 0 || dc_id == dc->id || !reply) &&
		    (!cstr_eq(cmd, "INVITE") ||
		     same(uc->casemap, irc_msg_param(msg, 0), uc->nick) ||
		     cap_registry_is_enabled(&dc->caps, "invite-notify")) &&
		    (!cstr_eq(cmd, "INVITE") || !cmmap_has(&net->blocked_targets, source) ||
		     strmap_has(&dc->metadata_subs, "soju.im/blocked"))) {
			struct irc_msg *copy = irc_msg_clone(msg);
			if (dc_id != 0 && dc->id != dc_id) {
				irc_tags_remove(&copy->tags, "label");
				irc_tags_remove(&copy->tags, "batch");
			}
			user_send_take(u, dc, copy);
		}
	}
out:
	irc_msg_free(msg);
}

void user_upstream_state(struct upstream *uc, void *data) {
	struct user *u = user_lookup(uc->srv, (int64_t)(uintptr_t)data);
	struct user_network *net = u != nullptr ? user_get_network(u, uc->network.id) : nullptr;
	if (net == nullptr || net->upstream != uc || u->stopped) {
		return;
	}
	cmmap_set_casemapping(&net->channels, uc->casemap);
	cmmap_set_casemapping(&net->delivered.targets, uc->casemap);
	cmmap_set_casemapping(&net->latest_times, uc->casemap);
	cmmap_set_casemapping(&net->push_targets, uc->casemap);
	cmmap_set_casemapping(&net->blocked_targets, uc->casemap);
	if (uc->attached && net->joined_generation != uc->generation) {
		net->joined_generation = uc->generation;
		xfree(net->last_error);
		net->last_error = xstrdup("");
		strvec channels = {0}, keys = {0};
		const char *name;
		void *v;
		for (size_t it = 0; cmmap_next(&net->channels, &it, &name, &v);) {
			struct db_channel *ch = v;
			strvec_push(&channels, name);
			strvec_push(&keys, ch->key);
		}
		irc_msg_vec joins = {0};
		generate_join(&joins, (const char *const *)channels.data,
			      (const char *const *)keys.data, channels.len);
		for (size_t i = 0; i < joins.len; i++) {
			upstream_send_take(uc, joins.data[i]);
		}
		vec_free(&joins);
		strvec_free(&channels);
		strvec_free(&keys);
	}
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *dc = u->clients.data[i];
		if (dc->network_id != net->record.id) {
			continue;
		}
		struct user_packet *p = user_packet_new(dc, nullptr);
		user_packet_sync(u, net, p, false);
		if (dc->id == uc->batch_downstream && uc->batch_ref != nullptr) {
			p->batch_tag = xstrdup(uc->batch_ref);
		}
		user_packet_send_take(p);
	}
	user_update_away_monitor(u, net);
	if (net->notified_attached != uc->attached) {
		net->notified_attached = uc->attached;
		user_notify_network_attrs(
			u, net, uc->attached ? "state=connected;error" : "state=disconnected");
	}
}

static void network_notice(struct user *u, struct user_network *net, const char *text,
			   bool bouncer_networks) {
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *dc = u->clients.data[i];
		if (dc->network_id != net->record.id ||
		    (!bouncer_networks &&
		     cap_registry_is_enabled(&dc->caps, "soju.im/bouncer-networks"))) {
			continue;
		}
		struct irc_msg *notice = IRC_MSG("NOTICE", dc->nick, text);
		irc_msg_set_prefix(notice, SERVICE_NICK, SERVICE_NICK, SERVICE_NICK);
		user_send_take(u, dc, notice);
	}
}

// soju-rs handle_upstream_disconnected and UpstreamConnectionError: reason
// is empty when the connection closed without error, e.g. after the server
// closed a registered connection
void user_upstream_closed(struct upstream *uc, const char *reason, bool temporary, void *data) {
	(void)temporary;
	struct user *u = user_lookup(uc->srv, (int64_t)(uintptr_t)data);
	struct user_network *net = u != nullptr ? user_get_network(u, uc->network.id) : nullptr;
	if (net == nullptr || net->upstream != uc || u->stopped) {
		return;
	}
	net->joined_generation = 0;
	const char *name = db_network_get_name(&net->record);
	if (uc->attached && net->last_error[0] == '\0') {
		AUTOFREE char *text = xasprintf("disconnected from %s", name);
		network_notice(u, net, text, false);
	}
	if (reason[0] == '\0') {
		return;
	}
	AUTOFREE char *err = xasprintf("connection error: %s", reason);
	if (net->record.enabled && !cstr_eq(net->last_error, err)) {
		AUTOFREE char *text =
			xasprintf("failed connecting/registering to %s: %s", name, err);
		network_notice(u, net, text, true);
	}
	xfree(net->last_error);
	net->last_error = xstrdup(err);
	struct irc_tags attrs = IRC_TAGS_INIT;
	irc_tags_set(&attrs, "error", err);
	AUTO_STRBUF struct strbuf text_attrs = STRBUF_INIT;
	irc_tags_format(&attrs, &text_attrs);
	user_notify_network_attrs(u, net, strbuf_cstr(&text_attrs));
	irc_tags_free(&attrs);
}

// soju-rs RPL_ENDOFNAMES: the channel's state for each downstream, unless
// detached
void user_upstream_channel_complete(struct upstream *uc, const char *channel, void *data) {
	struct user *u = user_lookup(uc->srv, (int64_t)(uintptr_t)data);
	struct user_network *net = u != nullptr ? user_get_network(u, uc->network.id) : nullptr;
	if (net == nullptr || net->upstream != uc || u->stopped) {
		return;
	}
	struct db_channel *saved = cmmap_get(&net->channels, channel);
	struct upstream_channel *ch = cmmap_get(&uc->channels, channel);
	if (ch == nullptr || (saved != nullptr && saved->detached)) {
		return;
	}
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *dc = u->clients.data[i];
		if (dc->network_id != net->record.id) {
			continue;
		}
		struct user_packet *p = user_packet_new(dc, nullptr);
		user_forward_channel(u, net, dc, p, ch);
		user_packet_send_take(p);
	}
}

void user_upstream_detach(struct upstream *uc, const char *channel, void *data) {
	struct user *u = user_lookup(uc->srv, (int64_t)(uintptr_t)data);
	struct user_network *net = u != nullptr ? user_get_network(u, uc->network.id) : nullptr;
	if (net != nullptr && net->upstream == uc) {
		user_detach_channel(u, net, channel);
	}
}
