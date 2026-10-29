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
	// Give every known client a baseline for a newly encountered target.
	if (!cmmap_has(&net->delivered.targets, target)) {
		struct strmap clients;
		strmap_init(&clients, nullptr);
		void *v;
		const char *name;
		for (size_t it = 0; cmmap_next(&net->delivered.targets, &it, nullptr, &v);) {
			for (size_t jt = 0; strmap_next(v, &jt, &name, nullptr);) {
				strmap_set(&clients, name, nullptr);
			}
		}
		for (size_t i = 0; i < u->clients.len; i++) {
			if (u->clients.data[i]->network_id == net->record.id) {
				strmap_set(&clients, u->clients.data[i]->client, nullptr);
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
	if (detached && user_network_highlight(u, net, msg) &&
	    (ch->reattach_on == FILTER_DEFAULT || ch->reattach_on == FILTER_HIGHLIGHT ||
	     ch->reattach_on == FILTER_MESSAGE)) {
		user_attach_channel(u, net, target);
		detached = false;
	}
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *dc = u->clients.data[i];
		if (dc->network_id != net->record.id) {
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
		} else if (detached && (ch->relay_detached == FILTER_MESSAGE ||
					(ch->relay_detached != FILTER_NONE &&
					 user_network_highlight(u, net, msg)))) {
			AUTOFREE char *text = xasprintf(
				"%s in %s: <%s> %s",
				user_network_highlight(u, net, msg) ? "highlight" : "message",
				target, irc_msg_prefix_name(msg), irc_msg_param(msg, 1));
			struct irc_msg *notice = IRC_MSG("NOTICE", dc->nick, text);
			AUTOFREE char *host = server_hostname(u->srv);
			irc_msg_set_prefix(notice, SERVICE_NICK, SERVICE_NICK, host);
			user_packet_add_take(p, notice);
		}
		if (id[0] != '\0' && !cap_registry_is_enabled(&dc->caps, "draft/chathistory") &&
		    (cstr_eq(msg->command, "PRIVMSG") || cstr_eq(msg->command, "NOTICE"))) {
			AUTOFREE char *ping = xasprintf("soju-msgid-%s", id);
			user_packet_add_take(p, IRC_MSG("PING", ping));
		}
		user_packet_send_take(p);
	}
}

void user_upstream_message(struct upstream *uc, struct irc_msg *msg, uint64_t dc_id, void *data) {
	struct user *u = user_lookup(uc->srv, (int64_t)(uintptr_t)data);
	struct user_network *net = u != nullptr ? user_get_network(u, uc->network.id) : nullptr;
	if (net == nullptr || net->upstream != uc || u->stopped) {
		goto out;
	}
	const char *source = irc_msg_prefix_name(msg), *cmd = msg->command;
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
		const char *buffer = same(uc->casemap, target, uc->nick) ? source : target;
		if (!uc->registered || same(uc->casemap, source, uc->server_name) ||
		    cstr_eq(target, "*") || target[0] == '$') {
			buffer = "";
		}
		const char *context = irc_tags_get(&msg->tags, "+draft/channel-context");
		struct upstream_channel *channel =
			context != nullptr ? cmmap_get(&uc->channels, context) : nullptr;
		if (channel != nullptr && cmmap_has(&channel->members, source)) {
			buffer = channel->name;
		}
		user_produce(u, net, msg, buffer, from_us ? dc_id : 0, dc_id);
		goto out;
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
	if ((cstr_eq(cmd, "NICK") && same(uc->casemap, irc_msg_param(msg, 0), uc->nick)) ||
	    ((cstr_eq(cmd, "CHGHOST") || cstr_eq(cmd, "SETNAME")) &&
	     same(uc->casemap, source, uc->nick))) {
		goto out;
	}
	const char *channel = nullptr;
	if (cstr_eq(cmd, "JOIN") || cstr_eq(cmd, "PART") || cstr_eq(cmd, "TOPIC") ||
	    cstr_eq(cmd, "MODE") || cstr_eq(cmd, "KICK")) {
		channel = irc_msg_param(msg, 0);
	} else if (cstr_eq(cmd, "332") || cstr_eq(cmd, "333") || cstr_eq(cmd, "366")) {
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
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *dc = u->clients.data[i];
		if (dc->network_id == net->record.id && (dc_id == 0 || dc_id == dc->id)) {
			user_send_take(u, dc, irc_msg_clone(msg));
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
	if (uc->attached && net->joined_generation != uc->generation) {
		net->joined_generation = uc->generation;
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
		user_packet_send_take(p);
	}
	user_update_away_monitor(u, net);
	user_notify_network(u, net);
}

void user_upstream_closed(struct upstream *uc, const char *reason, bool temporary, void *data) {
	(void)temporary;
	struct user *u = user_lookup(uc->srv, (int64_t)(uintptr_t)data);
	struct user_network *net = u != nullptr ? user_get_network(u, uc->network.id) : nullptr;
	if (net == nullptr || net->upstream != uc || u->stopped) {
		return;
	}
	xfree(net->last_error);
	net->last_error = xstrdup(reason);
	net->joined_generation = 0;
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *dc = u->clients.data[i];
		if (dc->network_id != net->record.id) {
			continue;
		}
		AUTOFREE char *text = xasprintf("disconnected from %s: %s",
						db_network_get_name(&net->record), reason);
		struct irc_msg *notice = IRC_MSG("NOTICE", dc->nick, text);
		AUTOFREE char *host = server_hostname(u->srv);
		irc_msg_set_prefix(notice, SERVICE_NICK, SERVICE_NICK, host);
		user_send_take(u, dc, notice);
	}
}

void user_upstream_detach(struct upstream *uc, const char *channel, void *data) {
	struct user *u = user_lookup(uc->srv, (int64_t)(uintptr_t)data);
	struct user_network *net = u != nullptr ? user_get_network(u, uc->network.id) : nullptr;
	if (net != nullptr && net->upstream == uc) {
		user_detach_channel(u, net, channel);
	}
}
