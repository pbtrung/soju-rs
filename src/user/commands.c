#include "user/internal.h"
#include "service/service.h"

#include "base/alloc.h"
#include "base/base64.h"
#include "irc/genmsg.h"
#include "irc/numerics.h"
#include "irc/whox.h"

static void fail(struct user_packet *p, const char *cmd, const char *code, const char *sub,
		 const char *text) {
	user_packet_add_take(p, IRC_MSG("FAIL", cmd, code, sub, text));
}
static bool enabled(const struct user_client *dc, const char *cap) {
	return cap_registry_is_enabled(&dc->caps, cap);
}
static void no_ack(struct user_packet *p) {
	irc_msg_free(p->command);
	p->command = nullptr;
}

static void bouncer(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		    struct user_packet *p) {
	const char *sub = irc_msg_param(msg, 0);
	if (cstr_eq(sub, "BIND")) {
		fail(p, "BOUNCER", "REGISTRATION_IS_COMPLETED", sub,
		     "Cannot bind to a network after registration");
		return;
	}
	if (cstr_eq(sub, "LISTNETWORKS")) {
		user_send_networks(u, p);
		return;
	}
	bool attach = cstr_eq(sub, "ATTACH"), detach = cstr_eq(sub, "DETACH");
	if ((attach || detach) && !enabled(dc, "soju.im/multiplex")) {
		fail(p, "BOUNCER", "UNKNOWN_COMMAND", sub, "Unknown subcommand");
		return;
	}
	if (attach || detach) {
		if (dc->network_id != 0 || dc->parent_id != 0) {
			fail(p, "BOUNCER", "INVALID_CONNECTION", sub,
			     "Networks can only be attached to the bouncer connection");
			return;
		}
	}
	if (cstr_eq(sub, "ADDNETWORK")) {
		struct db_network record;
		db_network_init(&record, "");
		struct irc_tags attrs = IRC_TAGS_INIT;
		irc_tags_parse(&attrs, str_from(irc_msg_param(msg, 1)));
		struct irc_msg *error = nullptr;
		struct err *err = nullptr;
		if (!update_network_attrs(&record, &attrs, sub, &error)) {
			user_packet_add_take(p, error);
		} else if (!user_store_network(u, &record, true, &err)) {
			AUTOFREE char *text =
				xasprintf("Failed to create network: %s", err_msg(err));
			fail(p, "BOUNCER", "UNKNOWN_ERROR", sub, text);
		} else {
			AUTOFREE char *id = xasprintf("%lld", (long long)record.id);
			user_packet_add_take(p, IRC_MSG("BOUNCER", "ADDNETWORK", id));
		}
		err_free(err);
		irc_tags_free(&attrs);
		db_network_free(&record);
		return;
	}
	if (!attach && !detach && !cstr_eq(sub, "CHANGENETWORK") && !cstr_eq(sub, "DELNETWORK")) {
		fail(p, "BOUNCER", "UNKNOWN_COMMAND", sub, "Unknown subcommand");
		return;
	}
	long long id;
	if (!str_parse_i64(str_from(irc_msg_param(msg, 1)), &id) || id <= 0) {
		user_packet_add_take(p, IRC_MSG("FAIL", "BOUNCER", "INVALID_NETID", sub,
						irc_msg_param(msg, 1), "Invalid network ID"));
		return;
	}
	struct user_network *net = user_get_network(u, id);
	if (detach) {
		p->detach = true;
		p->network_id = id;
		return;
	}
	if (net == nullptr) {
		user_packet_add_take(p, IRC_MSG("FAIL", "BOUNCER", "INVALID_NETID", sub,
						irc_msg_param(msg, 1), "Invalid network ID"));
		return;
	}
	if (attach) {
		p->attach = true;
		p->network_id = id;
		return;
	}
	struct err *err = nullptr;
	if (cstr_eq(sub, "DELNETWORK")) {
		if (!user_delete_network(u, id, &err)) {
			fail(p, "BOUNCER", "UNKNOWN_ERROR", sub, err_msg(err));
		} else {
			user_packet_add_take(
				p, IRC_MSG("BOUNCER", "DELNETWORK", irc_msg_param(msg, 1)));
		}
	} else if (cstr_eq(sub, "CHANGENETWORK")) {
		struct db_network record;
		db_network_copy(&record, &net->record);
		struct irc_tags attrs = IRC_TAGS_INIT;
		irc_tags_parse(&attrs, str_from(irc_msg_param(msg, 2)));
		struct irc_msg *error = nullptr;
		if (!update_network_attrs(&record, &attrs, sub, &error)) {
			user_packet_add_take(p, error);
		} else if (!user_store_network(u, &record, true, &err)) {
			AUTOFREE char *text =
				xasprintf("Failed to update network: %s", err_msg(err));
			fail(p, "BOUNCER", "UNKNOWN_ERROR", sub, text);
		} else {
			user_packet_add_take(
				p, IRC_MSG("BOUNCER", "CHANGENETWORK", irc_msg_param(msg, 1)));
		}
		irc_tags_free(&attrs);
		db_network_free(&record);
	} else {
		fail(p, "BOUNCER", "UNKNOWN_COMMAND", sub, "Unknown subcommand");
	}
	err_free(err);
}

static void monitor(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		    struct user_packet *p) {
	const char *sub = irc_msg_param(msg, 0);
	struct user_network *net = user_get_network(u, dc->network_id);
	if (cstr_eq(sub, "C")) {
		cmmap_clear(&dc->monitored);
	} else if (cstr_eq(sub, "+") || cstr_eq(sub, "-")) {
		struct str rest = str_from(irc_msg_param(msg, 1)), part;
		if (str_contains(rest, ' ') || str_contains(rest, ':')) {
			return;
		}
		while (str_next_field(&rest, ',', &part)) {
			if (part.len == 0) {
				continue;
			}
			AUTOFREE char *name = str_dup(part);
			if (cstr_eq(sub, "-")) {
				cmmap_del(&dc->monitored, name);
			} else if (cmmap_len(&dc->monitored) < 1000) {
				cmmap_set(&dc->monitored, name, nullptr);
			}
		}
	}
	if (cstr_eq(sub, "L")) {
		const char *name;
		for (size_t it = 0; cmmap_next(&dc->monitored, &it, &name, nullptr);) {
			user_packet_add_take(p, IRC_MSG("732", dc->nick, name));
		}
		user_packet_add_take(p, IRC_MSG("733", dc->nick, "End of MONITOR list"));
	} else if (cstr_eq(sub, "S") || cstr_eq(sub, "+")) {
		const char *name;
		for (size_t it = 0; cmmap_next(&dc->monitored, &it, &name, nullptr);) {
			AUTOFREE char *canonical = casemap_cstr(CASEMAPPING_ASCII, name);
			bool *up = net != nullptr && net->upstream != nullptr
					   ? cmmap_get(&net->upstream->monitored, name)
					   : nullptr;
			if (up != nullptr || cstr_eq(canonical, SERVICE_NICK_CM) ||
			    net == nullptr) {
				bool online = cstr_eq(canonical, SERVICE_NICK_CM) ||
					      (up != nullptr && *up);
				user_packet_add_take(
					p, IRC_MSG(online ? "730" : "731", dc->nick, name));
			}
		}
	}
	if (net != nullptr) {
		user_update_away_monitor(u, net);
	}
}

static bool local_query(struct user *u, struct user_client *dc, const struct irc_msg *msg,
			struct user_packet *p);
// The label is only used once: by the first upstream command, or the reply
static const char *take_label(struct user_packet *p) {
	if (p->command == nullptr) {
		return "";
	}
	const char *label = irc_tags_get(&p->command->tags, "label");
	if (label != nullptr) {
		xfree(p->label);
		p->label = xstrdup(label);
		irc_tags_remove(&p->command->tags, "label");
		return p->label;
	}
	return "";
}
// For commands mixing local and remote replies (soju-rs drop_label)
static void drop_label(struct user_packet *p) {
	if (p->command != nullptr) {
		irc_tags_remove(&p->command->tags, "label");
	}
}
static enum casemapping client_casemap(const struct user_network *net) {
	return net == nullptr             ? CASEMAPPING_ASCII
	       : net->upstream != nullptr ? net->upstream->casemap
					  : CASEMAPPING_RFC1459;
}
static bool same_name(enum casemapping cm, const char *a, const char *b) {
	AUTOFREE char *ca = casemap_cstr(cm, a);
	AUTOFREE char *cb = casemap_cstr(cm, b);
	return cstr_eq(ca, cb);
}
// soju-rs upstream_for_command: replies with an error if there's no
// upstream connection to send the command to
static struct upstream *upstream_for(struct user_network *net, struct user_packet *p,
				     const char *cmd) {
	if (net == nullptr) {
		user_packet_add_take(
			p, IRC_MSG("400", "*", cmd,
				   "Cannot interact with channels and users on the bouncer "
				   "connection. Did you mean to use a specific network?"));
		return nullptr;
	}
	if (net->upstream == nullptr || !net->upstream->attached) {
		user_packet_add_take(
			p, IRC_MSG("400", "*", cmd, "Disconnected from upstream network"));
		return nullptr;
	}
	return net->upstream;
}
static bool need_params(const struct irc_msg *msg, size_t n, struct user_packet *p) {
	if (msg->params.len >= n) {
		return true;
	}
	user_packet_add_take(p, IRC_MSG("461", "*", msg->command, "Not enough parameters"));
	return false;
}
static void send_labeled(struct upstream *uc, const struct user_client *dc, struct user_packet *p,
			 struct irc_msg *msg) {
	upstream_send_labeled_take(uc, dc->id, take_label(p), msg);
}
// Sends a state update to the clients of the bouncer connection
static void sync_bouncer_clients(struct user *u, bool nick_only) {
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *peer = u->clients.data[i];
		if (peer->network_id == 0) {
			struct user_packet *sync = user_packet_new(peer, nullptr);
			user_packet_sync(u, nullptr, sync, false);
			sync->state.nick_only = nick_only;
			user_packet_send_take(sync);
		}
	}
}

static void handle_nick(struct user *u, struct user_client *dc, const struct irc_msg *msg,
			struct user_packet *p) {
	if (!need_params(msg, 1, p)) {
		return;
	}
	const char *nick = irc_msg_param(msg, 0);
	struct user_network *net = user_get_network(u, dc->network_id);
	if (nick[0] == '\0' || strpbrk(nick, ILLEGAL_NICK_CHARS) != nullptr) {
		user_packet_add_take(
			p, IRC_MSG("432", dc->nick, nick, "Nickname contains illegal characters"));
		return;
	}
	if (same_name(client_casemap(net), nick, SERVICE_NICK)) {
		user_packet_add_take(
			p, IRC_MSG("433", dc->nick, nick, "Nickname reserved for bouncer service"));
		return;
	}
	struct err *err = nullptr;
	bool ok;
	if (net != nullptr) {
		struct db_network record;
		db_network_copy(&record, &net->record);
		xfree(record.nick);
		record.nick = xstrdup(nick);
		ok = user_save_network(u, net, &record, &err);
		db_network_free(&record);
	} else {
		struct db_user record;
		db_user_copy(&record, &u->record);
		xfree(record.nick);
		record.nick = xstrdup(nick);
		ok = user_save_record(u, &record, &err);
		db_user_free(&record);
	}
	if (!ok) {
		log_info(&u->srv->log, "failed to update nick: %s", err_msg(err));
		err_free(err);
		user_packet_add_take(p, IRC_MSG("400", dc->nick, "NICK", "Failed to update nick"));
		return;
	}
	if (net == nullptr) {
		sync_bouncer_clients(u, true);
	} else if (net->upstream != nullptr && net->upstream->attached) {
		send_labeled(net->upstream, dc, p, IRC_MSG("NICK", nick));
	} else {
		user_packet_sync(u, net, p, false);
	}
}

static void handle_setname(struct user *u, struct user_client *dc, const struct irc_msg *msg,
			   struct user_packet *p) {
	if (!need_params(msg, 1, p)) {
		return;
	}
	const char *realname = irc_msg_param(msg, 0);
	if (cstr_eq(dc->realname, realname)) {
		struct irc_msg *echo = IRC_MSG("SETNAME", realname);
		irc_msg_set_prefix(echo, DOWNSTREAM_SELF_PREFIX, "", "");
		user_packet_add_take(p, echo);
		return;
	}
	struct user_network *net = user_get_network(u, dc->network_id);
	struct err *err = nullptr;
	bool ok;
	if (net != nullptr) {
		// Resetting to the default just wipes the network's preference
		struct db_network record;
		db_network_copy(&record, &net->record);
		xfree(record.realname);
		record.realname = xstrdup(cstr_eq(realname, u->record.realname) ? "" : realname);
		struct upstream *uc = net->upstream;
		if (uc != nullptr && uc->attached &&
		    cap_registry_is_enabled(&uc->caps, "setname")) {
			// The server replies with SETNAME on success
			send_labeled(uc, dc, p, IRC_MSG("SETNAME", realname));
			ok = user_save_network(u, net, &record, &err);
		} else {
			// Reconnects
			ok = user_store_network(u, &record, false, &err);
		}
		db_network_free(&record);
	} else {
		struct db_user record;
		db_user_copy(&record, &u->record);
		xfree(record.realname);
		record.realname = xstrdup(realname);
		ok = user_save_record(u, &record, &err);
		db_user_free(&record);
	}
	if (!ok) {
		log_info(&u->srv->log, "failed to update realname: %s", err_msg(err));
		err_free(err);
		user_packet_add_take(p, IRC_MSG("FAIL", "SETNAME", "CANNOT_CHANGE_REALNAME",
						"Failed to update realname"));
		return;
	}
	if (net == nullptr) {
		sync_bouncer_clients(u, false);
	}
}

static void split_list(const char *list, strvec *out) {
	struct str rest = str_from(list), part;
	while (str_next_field(&rest, ',', &part)) {
		strvec_push_take(out, str_dup(part));
	}
}

static void handle_join(struct user *u, struct user_client *dc, const struct irc_msg *msg,
			struct user_packet *p) {
	struct user_network *net = user_get_network(u, dc->network_id);
	struct upstream *uc = upstream_for(net, p, msg->command);
	if (uc == nullptr || !need_params(msg, 1, p)) {
		return;
	}
	strvec names = {0}, keys = {0};
	split_list(irc_msg_param(msg, 0), &names);
	if (msg->params.len > 1) {
		split_list(irc_msg_param(msg, 1), &keys);
	}
	if (names.len > 1) {
		drop_label(p);
	}
	for (size_t i = 0; i < names.len; i++) {
		const char *name = names.data[i], *key = i < keys.len ? keys.data[i] : "";
		if (name[0] == '\0' || strpbrk(name, ILLEGAL_CHAN_CHARS) != nullptr) {
			user_packet_add_take(p, IRC_MSG(ERR_BADCHANMASK, dc->nick, name,
							"Invalid channel name"));
			continue;
		}
		if (!upstream_is_channel(uc, name)) {
			user_packet_add_take(p, IRC_MSG(ERR_NOSUCHCHANNEL, dc->nick, name,
							"Not a channel name"));
			continue;
		}
		// Clients may JOIN in bulk when reconnecting: don't flood the
		// server with the channels we're in
		if (!cmmap_has(&uc->channels, name)) {
			struct irc_msg *join = IRC_MSG("JOIN", name);
			if (key[0] != '\0') {
				irc_msg_add_param(join, key);
			}
			send_labeled(uc, dc, p, join);
		}
		struct db_channel *saved = cmmap_get(&net->channels, name);
		struct db_channel record;
		db_channel_init(&record);
		if (saved != nullptr) {
			// Don't clear the channel key if there's one set
			if (key[0] != '\0') {
				db_channel_free(&record);
				record = *saved;
				record.name = xstrdup(saved->name);
				record.key = xstrdup(key);
				record.detached_internal_msg_id =
					xstrdup(saved->detached_internal_msg_id);
				struct err *err = nullptr;
				if (!user_store_channel(u, net, &record, &err)) {
					log_info(&u->srv->log, "failed to update channel: %s",
						 err_msg(err));
					err_free(err);
				}
			}
			user_attach_channel(u, net, name);
		} else {
			xfree(record.name);
			record.name = xstrdup(name);
			xfree(record.key);
			record.key = xstrdup(key);
			struct err *err = nullptr;
			if (!user_store_channel(u, net, &record, &err)) {
				log_info(&u->srv->log, "failed to create channel: %s",
					 err_msg(err));
				err_free(err);
			}
		}
		db_channel_free(&record);
	}
	strvec_free(&names);
	strvec_free(&keys);
}

static void handle_part(struct user *u, struct user_client *dc, const struct irc_msg *msg,
			struct user_packet *p) {
	struct user_network *net = user_get_network(u, dc->network_id);
	struct upstream *uc = upstream_for(net, p, msg->command);
	if (uc == nullptr || !need_params(msg, 1, p)) {
		return;
	}
	const char *reason = msg->params.len > 1 ? irc_msg_param(msg, 1) : "";
	strvec names = {0};
	split_list(irc_msg_param(msg, 0), &names);
	if (names.len > 1) {
		drop_label(p);
	}
	for (size_t i = 0; i < names.len; i++) {
		const char *name = names.data[i];
		struct err *err = nullptr;
		if (strcasecmp(reason, "detach") == 0) {
			if (cmmap_has(&net->channels, name)) {
				user_detach_channel(u, net, name);
			} else {
				struct db_channel record;
				db_channel_init(&record);
				xfree(record.name);
				record.name = xstrdup(name);
				record.detached = true;
				if (!user_store_channel(u, net, &record, &err)) {
					log_info(&u->srv->log, "failed to create channel: %s",
						 err_msg(err));
					err_free(err);
				}
				db_channel_free(&record);
			}
		} else {
			struct irc_msg *part = IRC_MSG("PART", name);
			if (reason[0] != '\0') {
				irc_msg_add_param(part, reason);
			}
			send_labeled(uc, dc, p, part);
			if (!user_delete_channel(u, net, name, &err)) {
				log_info(&u->srv->log, "failed to delete channel: %s",
					 err_msg(err));
				err_free(err);
			}
			cmmap_del(&net->push_targets, name);
		}
	}
	strvec_free(&names);
}

static void send_topic(const struct user_client *dc, struct user_packet *p,
		       const struct upstream_channel *ch) {
	if (ch->topic[0] == '\0') {
		user_packet_add_take(p, IRC_MSG("331", dc->nick, ch->name, "No topic is set"));
		return;
	}
	user_packet_add_take(p, IRC_MSG("332", dc->nick, ch->name, ch->topic));
	if (ch->topic_who[0] != '\0') {
		AUTOFREE char *time =
			xasprintf("%lld", (long long)(ch->topic_time.set ? ch->topic_time.t.sec
									 : -62135596800LL));
		user_packet_add_take(p, IRC_MSG("333", dc->nick, ch->name, ch->topic_who, time));
	}
}

static void handle_mode(struct user *u, struct user_client *dc, const struct irc_msg *msg,
			struct user_packet *p) {
	if (!need_params(msg, 1, p)) {
		return;
	}
	const char *name = irc_msg_param(msg, 0);
	const char *modes = msg->params.len > 1 ? irc_msg_param(msg, 1) : "";
	struct user_network *net = user_get_network(u, dc->network_id);
	if (same_name(client_casemap(net), name, dc->nick)) {
		if (modes[0] != '\0') {
			struct upstream *uc = upstream_for(net, p, msg->command);
			if (uc != nullptr) {
				send_labeled(uc, dc, p, irc_msg_clone(msg));
			}
		} else {
			struct upstream *uc = net != nullptr ? net->upstream : nullptr;
			AUTOFREE char *reply = xasprintf("+%s", uc != nullptr && uc->attached
									? strbuf_cstr(&uc->modes.s)
									: "");
			user_packet_add_take(p, IRC_MSG("221", dc->nick, reply));
		}
		return;
	}
	struct upstream *uc = upstream_for(net, p, msg->command);
	if (uc == nullptr) {
		return;
	}
	if (!upstream_is_channel(uc, name)) {
		user_packet_add_take(p, IRC_MSG(ERR_USERSDONTMATCH, dc->nick,
						"Cannot change mode for other users"));
		return;
	}
	if (modes[0] != '\0') {
		struct irc_msg *mode = irc_msg_clone(msg);
		irc_msg_clear_prefix(mode);
		irc_tags_clear(&mode->tags);
		send_labeled(uc, dc, p, mode);
		return;
	}
	struct upstream_channel *ch = cmmap_get(&uc->channels, name);
	if (ch == nullptr) {
		// We're not on that channel
		send_labeled(uc, dc, p, IRC_MSG("MODE", name));
		return;
	}
	if (!ch->has_modes) {
		// The modes are broadcast when RPL_CHANNELMODEIS arrives
		return;
	}
	AUTO_STRBUF struct strbuf mode_str = STRBUF_INIT;
	strvec params = {0};
	format_channel_modes(&ch->modes, &mode_str, &params);
	struct irc_msg *reply = IRC_MSG("324", dc->nick, ch->name, strbuf_cstr(&mode_str));
	for (size_t i = 0; i < params.len; i++) {
		irc_msg_add_param(reply, params.data[i]);
	}
	strvec_free(&params);
	user_packet_add_take(p, reply);
	if (ch->creation_time[0] != '\0') {
		user_packet_add_take(p, IRC_MSG("329", dc->nick, ch->name, ch->creation_time));
	}
}

static void handle_topic(struct user *u, struct user_client *dc, const struct irc_msg *msg,
			 struct user_packet *p) {
	if (!need_params(msg, 1, p)) {
		return;
	}
	struct upstream *uc = upstream_for(user_get_network(u, dc->network_id), p, msg->command);
	if (uc == nullptr) {
		return;
	}
	const char *name = irc_msg_param(msg, 0);
	struct upstream_channel *ch = cmmap_get(&uc->channels, name);
	if (msg->params.len > 1) {
		send_labeled(uc, dc, p, IRC_MSG("TOPIC", name, irc_msg_param(msg, 1)));
	} else if (ch == nullptr) {
		send_labeled(uc, dc, p, IRC_MSG("TOPIC", name));
	} else {
		send_topic(dc, p, ch);
	}
}

static void handle_names(struct user *u, struct user_client *dc, const struct irc_msg *msg,
			 struct user_packet *p) {
	struct upstream *uc = upstream_for(user_get_network(u, dc->network_id), p, msg->command);
	if (uc == nullptr) {
		return;
	}
	if (msg->params.len == 0) {
		user_packet_add_take(p, IRC_MSG("366", dc->nick, "*", "End of /NAMES list"));
		return;
	}
	strvec names = {0};
	split_list(irc_msg_param(msg, 0), &names);
	if (names.len > 1) {
		drop_label(p);
	}
	for (size_t i = 0; i < names.len; i++) {
		struct upstream_channel *ch = cmmap_get(&uc->channels, names.data[i]);
		if (ch != nullptr) {
			user_names_reply(dc, p, ch);
		} else {
			send_labeled(uc, dc, p, IRC_MSG("NAMES", names.data[i]));
		}
	}
	strvec_free(&names);
}

// soju-rs get_cached_who: the reply from the users we know, if we know all
// the fields asked
static bool cached_who(struct upstream *uc, const struct user_client *dc, const char *mask,
		       const char *fields, const char *token, struct user_packet *p) {
	// Non-extended WHO fields
	const char *reply_fields = fields;
	if (fields[0] == '\0') {
		fields = "cuhsnfdr";
	}
	// Some capabilities are needed to keep the cached state in sync
	if ((strchr(fields, 'a') != nullptr &&
	     !cap_registry_is_enabled(&uc->caps, "account-notify")) ||
	    (strchr(fields, 'f') != nullptr &&
	     !cap_registry_is_enabled(&uc->caps, "away-notify"))) {
		return false;
	}
	const struct upstream_user *one = cmmap_get(&uc->users, mask);
	struct upstream_channel *ch = one == nullptr ? cmmap_get(&uc->channels, mask) : nullptr;
	VEC(const struct upstream_user *) users = {0};
	if (one != nullptr) {
		vec_push(&users, one);
	} else if (ch != nullptr) {
		const char *nick;
		for (size_t it = 0; cmmap_next(&ch->members, &it, &nick, nullptr);) {
			const struct upstream_user *member = cmmap_get(&uc->users, nick);
			if (member == nullptr) {
				vec_free(&users);
				return false;
			}
			vec_push(&users, member);
		}
	} else {
		return false;
	}
	for (size_t i = 0; i < users.len; i++) {
		if (!upstream_user_has_whox_fields(users.data[i], fields)) {
			vec_free(&users);
			return false;
		}
	}
	bool channel = upstream_is_channel(uc, mask);
	for (size_t i = 0; i < users.len; i++) {
		const struct upstream_user *uu = users.data[i];
		struct whox_info info;
		whox_info_init(&info);
		whox_info_set(&info, 't', token);
		whox_info_set(&info, 'u', uu->username);
		whox_info_set(&info, 'h', uu->hostname);
		whox_info_set(&info, 's', uu->server);
		whox_info_set(&info, 'n', uu->nickname);
		whox_info_set(&info, 'a', uu->account);
		whox_info_set(&info, 'r', uu->realname);
		AUTO_STRBUF struct strbuf flags = STRBUF_INIT;
		strbuf_append_cstr(&flags, uu->flags);
		if (channel) {
			whox_info_set(&info, 'c', mask);
			// The membership prefixes from the cached NAMES reply, after
			// the away status and server operator flags
			struct membership_set *ms =
				ch != nullptr ? cmmap_get(&ch->members, uu->nickname) : nullptr;
			if (ms != nullptr) {
				AUTO_STRBUF struct strbuf prefixes = STRBUF_INIT;
				format_member_prefix(
					&prefixes, ms,
					cap_registry_is_enabled(&dc->caps, "multi-prefix"));
				size_t at = strspn(uu->flags, "GH*");
				strbuf_free(&flags);
				strbuf_append(&flags, uu->flags, at);
				strbuf_append_str(&flags, strbuf_str(&prefixes));
				strbuf_append_cstr(&flags, uu->flags + at);
			}
		}
		whox_info_set(&info, 'f', strbuf_cstr(&flags));
		user_packet_add_take(p, generate_whox_reply(reply_fields, &info));
		whox_info_free(&info);
	}
	vec_free(&users);
	user_packet_add_take(p, IRC_MSG("315", "*", mask, "End of /WHO list"));
	return true;
}

static void handle_who(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		       struct user_packet *p) {
	if (msg->params.len == 0) {
		user_packet_add_take(p, IRC_MSG("315", "*", "*", "End of /WHO list"));
		return;
	}
	if (local_query(u, dc, msg, p)) {
		return;
	}
	struct upstream *uc = upstream_for(user_get_network(u, dc->network_id), p, msg->command);
	if (uc == nullptr) {
		return;
	}
	char *fields, *token;
	parse_whox_options(str_from(irc_msg_param(msg, 1)), &fields, &token);
	bool cached = cached_who(uc, dc, irc_msg_param(msg, 0), fields, token, p);
	xfree(fields);
	xfree(token);
	if (!cached) {
		bool queued = upstream_enqueue_take(uc, dc->id, take_label(p), irc_msg_clone(msg));
		(void)queued;
	}
}

static bool local_query(struct user *u, struct user_client *dc, const struct irc_msg *msg,
			struct user_packet *p) {
	const char *mask =
		cstr_eq(msg->command, "WHO") ? irc_msg_param(msg, 0) : irc_msg_trailing(msg);
	AUTOFREE char *cm = casemap_cstr(CASEMAPPING_ASCII, mask);
	bool service = cstr_eq(cm, SERVICE_NICK_CM);
	if (!service && dc->network_id != 0) {
		return false;
	}
	bool self = cstr_eq(mask, dc->nick);
	AUTOFREE char *host = server_hostname(u->srv);
	if (cstr_eq(msg->command, "WHOIS")) {
		if (service || self) {
			user_packet_add_take(p, IRC_MSG("311", dc->nick, mask,
							service ? SERVICE_NICK : u->record.username,
							service ? SERVICE_NICK : dc->hostname, "*",
							service ? SERVICE_REALNAME : dc->realname));
			user_packet_add_take(p, IRC_MSG("312", dc->nick, mask, host, "soju"));
			if (service || u->record.admin) {
				user_packet_add_take(
					p, IRC_MSG("313", dc->nick, mask,
						   service ? "is the bouncer service"
							   : "is a bouncer administrator"));
			}
			user_packet_add_take(p, IRC_MSG("330", dc->nick, mask,
							service ? SERVICE_NICK : u->record.username,
							"is logged in as"));
			if (service) {
				user_packet_add_take(p, IRC_MSG("335", dc->nick, mask, "is a bot"));
			}
		} else {
			user_packet_add_take(
				p, IRC_MSG("401", dc->nick, mask, "No such nick/channel"));
		}
		user_packet_add_take(p, IRC_MSG("318", dc->nick, mask, "End of /WHOIS list"));
	} else if (cstr_eq(msg->command, "WHO")) {
		if (service || self) {
			char *fields, *token;
			parse_whox_options(str_from(irc_msg_param(msg, 1)), &fields, &token);
			struct whox_info info;
			whox_info_init(&info);
			whox_info_set(&info, 't', token);
			whox_info_set(&info, 'n', service ? SERVICE_NICK : dc->nick);
			whox_info_set(&info, 'u', service ? SERVICE_NICK : u->record.username);
			whox_info_set(&info, 'h', service ? SERVICE_NICK : dc->hostname);
			whox_info_set(&info, 's', host);
			whox_info_set(&info, 'f',
				      service               ? "H*B"
				      : u->record.admin     ? "H*"
				      : dc->away != nullptr ? "G"
							    : "H");
			whox_info_set(&info, 'a', service ? SERVICE_NICK : u->record.username);
			whox_info_set(&info, 'r', service ? SERVICE_REALNAME : dc->realname);
			user_packet_add_take(p, generate_whox_reply(fields, &info));
			whox_info_free(&info);
			xfree(fields);
			xfree(token);
		}
		user_packet_add_take(
			p, IRC_MSG("315", "*", irc_msg_param(msg, 0), "End of /WHO list"));
	} else {
		return false;
	}
	return true;
}

// soju-rs auto_save_sasl_plain: e.g. after a NickServ REGISTER. Other
// mechanisms (e.g. EXTERNAL) are kept.
void user_auto_save_sasl_plain(struct user *u, struct user_network *net, const char *username,
			       const char *password) {
	if (net->record.sasl.mechanism[0] != '\0' &&
	    !cstr_eq(net->record.sasl.mechanism, "PLAIN")) {
		return;
	}
	struct db_network record;
	db_network_copy(&record, &net->record);
	xfree(record.sasl.mechanism);
	record.sasl.mechanism = xstrdup("PLAIN");
	xfree(record.sasl.plain_username);
	record.sasl.plain_username = xstrdup(username);
	xfree(record.sasl.plain_password);
	record.sasl.plain_password = xstrdup(password);
	struct err *err = nullptr;
	if (!user_save_network(u, net, &record, &err)) {
		log_info(&u->srv->log, "failed to save SASL PLAIN credentials: %s", err_msg(err));
		err_free(err);
	}
	db_network_free(&record);
}

// Erases credentials (an empty response has no buffer)
static void wipe(struct strbuf *b) {
	if (b->buf != nullptr) {
		explicit_bzero(b->buf, b->len);
	}
}

// soju-rs handle_authenticate_registered: SASL PLAIN with the upstream
// network, after registration
static void authenticate(struct user_client *dc, struct user_network *net,
			 const struct irc_msg *msg, struct user_packet *p) {
	struct upstream *uc = net->upstream;
	if (uc == nullptr || !uc->attached || !cap_registry_is_enabled(&uc->caps, "sasl")) {
		dc->sasl_plain = false;
		user_packet_add_take(p, IRC_MSG("904", dc->nick,
						"Upstream network authentication not supported"));
		return;
	}
	if (!need_params(msg, 1, p)) {
		return;
	}
	const char *arg = irc_msg_param(msg, 0);
	if (cstr_eq(arg, "*")) {
		dc->sasl_plain = false;
		user_packet_add_take(p, IRC_MSG("906", dc->nick, "SASL authentication aborted"));
		return;
	}
	if (!dc->sasl_plain) {
		if (strcasecmp(arg, "PLAIN") != 0) {
			user_packet_add_take(p,
					     IRC_MSG("904", dc->nick,
						     "Unsupported SASL authentication mechanism"));
			return;
		}
		dc->sasl_plain = true;
		struct irc_msg *challenge = IRC_MSG("AUTHENTICATE", "+");
		irc_msg_set_prefix(challenge, "*", "", "");
		user_packet_add_take(p, challenge);
		return;
	}
	dc->sasl_plain = false;
	AUTO_STRBUF struct strbuf decoded = STRBUF_INIT;
	if (!cstr_eq(arg, "+") &&
	    !base64_decode(&decoded, BASE64_STANDARD, str_from(arg), nullptr)) {
		user_packet_add_take(p, IRC_MSG("904", dc->nick, "Invalid SASL response"));
		return;
	}
	// identity NUL username NUL password
	struct str rest = strbuf_str(&decoded), identity, username, password;
	if (!str_split_once(rest, '\0', &identity, &rest) ||
	    !str_split_once(rest, '\0', &username, &password)) {
		user_packet_add_take(p, IRC_MSG("904", dc->nick, "Invalid SASL PLAIN response"));
		wipe(&decoded);
		return;
	}
	if (identity.len > 0 && !str_eq(identity, username)) {
		user_packet_add_take(p,
				     IRC_MSG("904", dc->nick, "SASL PLAIN identity not supported"));
	} else if (uc->sasl_active || uc->sasl_username != nullptr) {
		user_packet_add_take(p, IRC_MSG("904", dc->nick,
						"Another authentication attempt is already in "
						"progress"));
	} else {
		uc->sasl_username = str_dup(username);
		uc->sasl_password = str_dup(password);
		uc->sasl_active = true;
		uc->sasl_started = false;
		bool queued = upstream_enqueue_take(uc, dc->id, take_label(p),
						    IRC_MSG("AUTHENTICATE", "PLAIN"));
		(void)queued;
		no_ack(p);
	}
	wipe(&decoded);
}

// soju-rs handle_privmsg
static void privmsg(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		    struct user_packet *p) {
	const char *cmd = msg->command;
	bool text = !cstr_eq(cmd, "TAGMSG");
	if (!need_params(msg, text ? 2 : 1, p)) {
		return;
	}
	struct user_network *net = user_get_network(u, dc->network_id);
	strvec targets = {0};
	split_list(irc_msg_param(msg, 0), &targets);
	if (targets.len > 1) {
		drop_label(p);
	}
	AUTOFREE char *host = server_hostname(u->srv);
	AUTOFREE char *server_mask = xasprintf("$%s", host);
	for (size_t i = 0; i < targets.len; i++) {
		const char *name = targets.data[i];
		struct irc_msg *out = irc_msg_clone(msg);
		irc_msg_clear_prefix(out);
		irc_tags_clear(&out->tags);
		copy_client_tags(&out->tags, &msg->tags);
		irc_msg_set_param(out, 0, name);
		AUTOFREE char *now = format_server_time_cstr(timestamp_now());
		if (cstr_eq(name, server_mask) || (cstr_eq(name, "$*") && net == nullptr)) {
			// A server mask: the bouncer's hostname broadcasts the
			// message to all the users
			if (!u->record.admin) {
				user_packet_add_take(p, IRC_MSG("415", dc->nick, name,
								"Permission denied to broadcast "
								"message to all bouncer users"));
				irc_msg_free(out);
				break;
			}
			irc_tags_set(&out->tags, "time", now);
			irc_msg_set_prefix(out, SERVICE_NICK, SERVICE_NICK, SERVICE_NICK);
			user_broadcast_take(u->srv, out);
			continue;
		}
		if (net == nullptr && same_name(CASEMAPPING_ASCII, name, dc->nick)) {
			irc_msg_free(out);
			struct irc_msg *echo = irc_msg_clone(msg);
			irc_msg_set_param(echo, 0, name);
			irc_msg_set_prefix(echo, DOWNSTREAM_SELF_PREFIX, "", "");
			user_packet_add_take(p, echo);
			continue;
		}
		if (same_name(CASEMAPPING_ASCII, name, SERVICE_NICK)) {
			if (enabled(dc, "echo-message")) {
				struct irc_msg *echo = irc_msg_clone(out);
				irc_tags_set(&echo->tags, "time", now);
				irc_msg_set_prefix(echo, DOWNSTREAM_SELF_PREFIX, "", "");
				user_packet_add_take(p, echo);
			}
			irc_msg_free(out);
			if (cstr_eq(cmd, "PRIVMSG")) {
				// Each line of a multiline message is a command
				struct str rest = str_from(irc_msg_param(msg, 1)), line;
				while (str_next_field(&rest, '\n', &line)) {
					AUTOFREE char *l = str_dup(line);
					if (l[strspn(l, " \t\r")] != '\0') {
						service_privmsg(u, dc, l, p);
					}
				}
			}
			continue;
		}
		struct upstream *uc = upstream_for(net, p, cmd);
		if (uc == nullptr) {
			irc_msg_free(out);
			break;
		}
		// The STATUSMSG sigils stay in the message: "@#chan" only reaches
		// channel operators
		const char *target = name;
		while (target[0] != '\0' && strchr(uc->status_msg, target[0]) != nullptr) {
			target++;
		}
		const char *body = text ? irc_msg_param(msg, 1) : "";
		char *account, *password;
		if (cstr_eq(cmd, "PRIVMSG") && strchr(body, '\n') == nullptr &&
		    same_name(uc->casemap, target, "NickServ") &&
		    parse_nickserv_credentials(body, uc->nick, &account, &password)) {
			user_auto_save_sasl_plain(u, net, account, password);
			xfree(account);
			xfree(password);
		}
		// Without echo-message, the message is produced here, with the
		// label of the command
		bool echo = cap_registry_is_enabled(&uc->caps, "echo-message");
		if (!echo) {
			struct irc_msg *local = irc_msg_clone(out);
			irc_tags_set(&local->tags, "time", now);
			if (uc->account[0] != '\0') {
				irc_tags_set(&local->tags, "account", uc->account);
			}
			irc_msg_set_prefix(local, uc->nick, uc->username, uc->hostname);
			user_produce(u, net, local, target, dc->id, dc->id);
			irc_msg_free(local);
		}
		upstream_send_labeled_take(uc, dc->id, echo ? take_label(p) : "", out);
	}
	strvec_free(&targets);
}

void user_handle_command(struct user *u, struct user_client *dc, struct irc_msg *msg,
			 uint64_t ack_id) {
	struct user_packet *p = user_packet_new(dc, msg);
	p->ack = true;
	p->ack_id = ack_id;
	struct user_network *net = user_get_network(u, dc->network_id);
	struct upstream *uc = net != nullptr && net->upstream != nullptr && net->upstream->attached
				      ? net->upstream
				      : nullptr;
	const char *cmd = msg->command;
	if (cstr_eq(cmd, "BATCH")) {
		const char *ref = irc_msg_param(msg, 0);
		if (ref[0] == '+' && cstr_eq(irc_msg_param(msg, 1), MULTILINE_BATCH_TYPE) &&
		    enabled(dc, MULTILINE_BATCH_TYPE)) {
			if (strmap_len(&dc->multiline) < 64 &&
			    !strmap_has(&dc->multiline, ref + 1)) {
				struct multiline_batch *b = xcalloc(1, sizeof(*b));
				multiline_batch_init(b, irc_msg_clone(msg));
				strmap_set(&dc->multiline, ref + 1, b);
			}
			no_ack(p);
			goto out;
		} else if (ref[0] == '-') {
			struct multiline_batch *b = strmap_take(&dc->multiline, ref + 1);
			if (b == nullptr) {
				goto out;
			}
			irc_msg_free(p->command);
			p->command = irc_msg_clone(b->opener);
			enum multiline_error error;
			struct irc_msg *logical = multiline_batch_finish(b, &error);
			xfree(b);
			if (logical == nullptr) {
				struct multiline_limits limits =
					uc != nullptr
						? multiline_limits_parse(str_from(
							  cap_registry_value(
								  &uc->caps,
								  MULTILINE_BATCH_TYPE) != nullptr
								  ? cap_registry_value(
									    &uc->caps,
									    MULTILINE_BATCH_TYPE)
								  : ""))
						: multiline_limits_default();
				AUTOFREE char *bound = xasprintf("%zu", error == MULTILINE_MAX_LINES
										? limits.max_lines
										: limits.max_bytes);
				user_packet_add_take(p, IRC_MSG("FAIL", "BATCH",
								multiline_error_code(error), bound,
								"Invalid multiline batch"));
				goto out;
			}
			irc_msg_free(msg);
			msg = logical;
			cmd = msg->command;
		}
	} else {
		const char *ref = irc_tags_get(&msg->tags, "batch");
		struct multiline_batch *b =
			ref != nullptr ? strmap_get(&dc->multiline, ref) : nullptr;
		if (b != nullptr) {
			const char *value =
				uc != nullptr ? cap_registry_value(&uc->caps, MULTILINE_BATCH_TYPE)
					      : nullptr;
			struct multiline_limits limits =
				value != nullptr ? multiline_limits_parse(str_from(value))
						 : multiline_limits_default();
			multiline_batch_push(b, msg, &limits);
			no_ack(p);
			goto out;
		}
	}
	if (cstr_eq(cmd, "PING")) {
		AUTOFREE char *host = server_hostname(u->srv);
		if (msg->params.len > 1 && !cstr_eq(irc_msg_param(msg, 1), host)) {
			user_packet_add_take(p, IRC_MSG("402", dc->nick, irc_msg_param(msg, 1),
							"No such server"));
		} else {
			user_packet_add_take(p, IRC_MSG("PONG", host, irc_msg_param(msg, 0)));
		}
	} else if (cstr_eq(cmd, "PONG")) {
		const char *token = irc_msg_trailing(msg);
		static constexpr char prefix[] = "soju-msgid-";
		if (net != nullptr && str_starts_with(str_from(token), STR_LIT("soju-msgid-"))) {
			int64_t id;
			AUTO_STRBUF struct strbuf target = STRBUF_INIT;
			if (parse_msg_id(str_from(token + sizeof(prefix) - 1), &id, &target,
					 nullptr) &&
			    id == net->record.id &&
			    memchr(target.buf, '\0', target.len) == nullptr) {
				delivered_set(&net->delivered, strbuf_cstr(&target), dc->client,
					      token + sizeof(prefix) - 1);
			}
		}
	} else if (cstr_eq(cmd, "BOUNCER")) {
		bouncer(u, dc, msg, p);
	} else if (cstr_eq(cmd, "METADATA")) {
		user_metadata(u, dc, msg, p);
		irc_msg_free(msg);
		return;
	} else if (cstr_eq(cmd, "CHATHISTORY")) {
		user_history(u, dc, msg, p);
	} else if (cstr_eq(cmd, "SEARCH")) {
		user_search(u, dc, msg, p);
	} else if (cstr_eq(cmd, "MARKREAD") || cstr_eq(cmd, "READ")) {
		user_read_marker(u, dc, msg, p);
	} else if (cstr_eq(cmd, "MONITOR")) {
		monitor(u, dc, msg, p);
	} else if (cstr_eq(cmd, "WEBPUSH")) {
		if (user_webpush_command(u, dc, msg, p)) {
			no_ack(p);
		}
	} else if (cstr_eq(cmd, "AWAY")) {
		xfree(dc->away);
		dc->away = msg->params.len > 0 ? xstrdup(irc_msg_param(msg, 0)) : nullptr;
		user_packet_add_take(
			p, IRC_MSG(dc->away != nullptr ? "306" : "305", "*",
				   dc->away != nullptr ? "You have been marked as being away"
						       : "You are no longer marked as being away"));
		if (net != nullptr) {
			user_update_away_monitor(u, net);
		}
	} else if (cstr_eq(cmd, "NICK")) {
		handle_nick(u, dc, msg, p);
	} else if (cstr_eq(cmd, "SETNAME")) {
		handle_setname(u, dc, msg, p);
	} else if (cstr_eq(cmd, "JOIN")) {
		handle_join(u, dc, msg, p);
	} else if (cstr_eq(cmd, "PART")) {
		handle_part(u, dc, msg, p);
	} else if (cstr_eq(cmd, "MODE")) {
		handle_mode(u, dc, msg, p);
	} else if (cstr_eq(cmd, "TOPIC")) {
		handle_topic(u, dc, msg, p);
	} else if (cstr_eq(cmd, "NAMES")) {
		handle_names(u, dc, msg, p);
	} else if (cstr_eq(cmd, "WHO")) {
		handle_who(u, dc, msg, p);
	} else if (cstr_eq(cmd, "WHOIS") && local_query(u, dc, msg, p)) {
		// Answered locally.
	} else if (cstr_eq(cmd, "KICK") || cstr_eq(cmd, "INVITE")) {
		struct upstream *target = upstream_for(net, p, cmd);
		if (target != nullptr) {
			send_labeled(target, dc, p, irc_msg_clone(msg));
		}
	} else if ((cstr_eq(cmd, "REGISTER") && msg->params.len < 3) ||
		   (cstr_eq(cmd, "VERIFY") && msg->params.len < 2)) {
		// The parameters are used to save the credentials on success
		user_packet_add_take(p, IRC_MSG("461", "*", cmd, "Not enough parameters"));
	} else if (cstr_eq(cmd, "PRIVMSG") || cstr_eq(cmd, "NOTICE") || cstr_eq(cmd, "TAGMSG")) {
		privmsg(u, dc, msg, p);
	} else if (cstr_eq(cmd, "AUTHENTICATE") && net != nullptr) {
		authenticate(dc, net, msg, p);
	} else if (uc != nullptr) {
		if (cstr_eq(cmd, "WHOIS") || cstr_eq(cmd, "LIST") || cstr_eq(cmd, "REGISTER") ||
		    cstr_eq(cmd, "VERIFY")) {
			bool queued = upstream_enqueue_take(uc, dc->id, take_label(p),
							    irc_msg_clone(msg));
			(void)queued;
		} else if (cstr_eq(cmd, "PASS") || cstr_eq(cmd, "USER")) {
			user_packet_add_take(p, IRC_MSG("462", dc->nick, "You may not reregister"));
			goto out;
		} else {
			send_labeled(uc, dc, p, irc_msg_clone(msg));
		}
		no_ack(p);
	} else if (net != nullptr && !cstr_eq(cmd, "PASS") && !cstr_eq(cmd, "USER")) {
		user_packet_add_take(
			p, IRC_MSG("421", "*", cmd, "Disconnected from upstream network"));
	} else if (cstr_eq(cmd, "MOTD")) {
		struct server_config *cfg = server_config_get(u->srv);
		if (cfg->motd[0] != '\0') {
			generate_motd(&p->messages, str_from(cfg->motd));
		} else {
			user_packet_add_take(p, IRC_MSG("422", dc->nick, "No MOTD"));
		}
		server_config_unref(cfg);
	} else if (cstr_eq(cmd, "PASS") || cstr_eq(cmd, "USER")) {
		user_packet_add_take(p, IRC_MSG("462", dc->nick, "You may not reregister"));
	} else if (cstr_eq(cmd, "INFO")) {
		user_packet_add_take(p, IRC_MSG("371", dc->nick, "soju <https://soju.im>"));
		user_packet_add_take(p, IRC_MSG("374", dc->nick, "End of INFO"));
	} else if (cstr_eq(cmd, "LIST")) {
		user_packet_add_take(p, IRC_MSG("323", dc->nick, "End of /LIST"));
	} else if (cstr_eq(cmd, "AUTHENTICATE")) {
		user_packet_add_take(
			p, IRC_MSG("907", dc->nick, "You have already authenticated using SASL"));
	} else {
		user_packet_add_take(p, IRC_MSG("421", "*", cmd, "Unknown command"));
	}
out:
	for (size_t i = 0; i < p->messages.len; i++) {
		// Like soju-rs, BOUNCER and CHATHISTORY TARGETS replies have none
		if (p->messages.data[i]->prefix == nullptr &&
		    !cstr_eq(p->messages.data[i]->command, "BOUNCER") &&
		    !cstr_eq(p->messages.data[i]->command, "CHATHISTORY")) {
			server_set_prefix(u->srv, p->messages.data[i]);
		}
	}
	user_packet_send_take(p);
	irc_msg_free(msg);
}
