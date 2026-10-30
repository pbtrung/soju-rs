// Users' state: nicknames, WHO replies, MONITOR, away and account status
// (soju-rs upstream/users.rs)
#include "upstream/state.h"

#include "base/alloc.h"
#include "irc/whox.h"

void upstream_free_user(void *v) {
	struct upstream_user *u = v;
	xfree(u->nickname);
	xfree(u->username);
	xfree(u->hostname);
	xfree(u->server);
	xfree(u->flags);
	xfree(u->account);
	xfree(u->realname);
	xfree(u);
}

// soju-rs cache_user_info: create is false for messages which only update
// the info of a user we already know
struct upstream_user *upstream_cache_user(struct upstream *uc, const struct irc_prefix *prefix,
					  bool create) {
	if (prefix == nullptr || prefix->name[0] == '\0') {
		return nullptr;
	}
	struct upstream_user *u = cmmap_get(&uc->users, prefix->name);
	if (u == nullptr) {
		if (!create) {
			return nullptr;
		}
		u = xcalloc(1, sizeof(*u));
		u->nickname = xstrdup(prefix->name);
		u->username = xstrdup("");
		u->hostname = xstrdup("");
		u->server = xstrdup("");
		u->flags = xstrdup("");
		u->account = xstrdup("");
		u->realname = xstrdup("");
		cmmap_set(&uc->users, prefix->name, u);
	}
	if (prefix->user[0] != '\0') {
		replace(&u->username, prefix->user);
	}
	if (prefix->host[0] != '\0') {
		replace(&u->hostname, prefix->host);
	}
	return u;
}

bool upstream_user_has_whox_fields(const struct upstream_user *u, const char *fields) {
	for (size_t i = 0; fields[i] != '\0'; i++) {
		const char *v = nullptr;
		switch (fields[i]) {
		case 'n':
			v = u->nickname;
			break;
		case 'u':
			v = u->username;
			break;
		case 'h':
			v = u->hostname;
			break;
		case 's':
			v = u->server;
			break;
		case 'f':
			v = u->flags;
			break;
		case 'a':
			v = u->account;
			break;
		case 'r':
			v = u->realname;
			break;
		default:
			continue;
		}
		if (v == nullptr || v[0] == '\0') {
			return false;
		}
	}
	return true;
}

// soju-rs should_cache_user_info: we keep the info of the users we MONITOR
// or share a channel with
bool upstream_should_cache(struct upstream *uc, const char *nick) {
	if (same(uc, nick, uc->nick) || cmmap_has(&uc->monitored, nick)) {
		return true;
	}
	void *v;
	for (size_t it = 0; cmmap_next(&uc->channels, &it, nullptr, &v);) {
		if (cmmap_has(&((struct upstream_channel *)v)->members, nick)) {
			return true;
		}
	}
	return false;
}

void upstream_forget_user(struct upstream *uc, const char *nick) {
	if (!upstream_should_cache(uc, nick)) {
		cmmap_del(&uc->users, nick);
	}
}

// 352, 354
bool upstream_handle_who_reply(struct upstream *uc, struct irc_msg *msg) {
	const char *cmd = msg->command;
	struct whox_info info;
	whox_info_init(&info);
	bool valid = false;
	if (cstr_eq(cmd, "352") && need(msg, 8)) {
		const char *real = strchr(irc_msg_param(msg, 7), ' ');
		if (real != nullptr) {
			whox_info_set(&info, 'n', irc_msg_param(msg, 5));
			whox_info_set(&info, 'u', irc_msg_param(msg, 2));
			whox_info_set(&info, 'h', irc_msg_param(msg, 3));
			whox_info_set(&info, 's', irc_msg_param(msg, 4));
			whox_info_set(&info, 'f', irc_msg_param(msg, 6));
			whox_info_set(&info, 'r', real + 1);
			valid = true;
		}
	} else if (cstr_eq(cmd, "354")) {
		pending_command_vec *q = strmap_get(&uc->pending, "WHO");
		if (q != nullptr && q->len > 0) {
			char *fields, *token;
			parse_whox_options(str_from(irc_msg_param(q->data[0].msg, 1)), &fields,
					   &token);
			if (strchr(fields, 'n') != nullptr) {
				struct whox_info parsed;
				if (parse_whox_reply(msg, fields, &parsed, nullptr)) {
					whox_info_free(&info);
					info = parsed;
					valid = true;
				}
			}
			xfree(fields);
			xfree(token);
		}
	}
	if (valid) {
		struct irc_prefix prefix = {
			.name = info.nickname, .user = info.username, .host = info.hostname};
		struct upstream_user *u = upstream_should_cache(uc, info.nickname)
						  ? upstream_cache_user(uc, &prefix, true)
						  : nullptr;
		if (u != nullptr) {
			if (info.server[0] != '\0') {
				replace(&u->server, info.server);
			}
			if (info.flags[0] != '\0') {
				AUTO_STRBUF struct strbuf flags = STRBUF_INIT;
				strip_member_prefixes(&flags, str_from(info.flags),
						      uc->memberships.data, uc->memberships.len);
				replace(&u->flags, strbuf_cstr(&flags));
			}
			if (info.account[0] != '\0') {
				replace(&u->account, info.account);
			}
			if (info.realname[0] != '\0') {
				replace(&u->realname, info.realname);
			}
		}
	}
	whox_info_free(&info);
	return valid;
}

// 730, 731
bool upstream_handle_monitor(struct upstream *uc, struct irc_msg *msg) {
	const char *cmd = msg->command;
	if (!need(msg, 2)) {
		return false;
	}
	struct str rest = str_from(irc_msg_param(msg, 1)), part;
	while (str_next_field(&rest, ',', &part)) {
		struct str name, user, host;
		irc_prefix_split(part, &name, &user, &host);
		AUTOFREE char *n = str_dup(name);
		if (n[0] == '\0') {
			continue;
		}
		bool *online = xmalloc(sizeof(*online));
		*online = cstr_eq(cmd, "730");
		cmmap_set(&uc->monitored, n, online);
		if (!*online && same(uc, n, db_get_nick(&uc->user, &uc->network)) &&
		    !same(uc, n, uc->nick)) {
			upstream_send_take(uc,
					   IRC_MSG("NICK", db_get_nick(&uc->user, &uc->network)));
		}
	}
	return true;
}

// QUIT, NICK
bool upstream_handle_quit_nick(struct upstream *uc, struct irc_msg *msg) {
	const char *cmd = msg->command, *source = irc_msg_prefix_name(msg);
	bool nick = cstr_eq(cmd, "NICK");
	if (source[0] == '\0' || (nick && !need(msg, 1))) {
		return false;
	}
	strvec_free(&uc->event_channels);
	void *v;
	for (size_t it = 0; cmmap_next(&uc->channels, &it, nullptr, &v);) {
		struct upstream_channel *ch = v;
		struct membership_set *ms = cmmap_take(&ch->members, source);
		if (ms != nullptr) {
			strvec_push(&uc->event_channels, ch->name);
			if (nick) {
				cmmap_set(&ch->members, irc_msg_param(msg, 0), ms);
			} else {
				upstream_free_member(ms);
			}
		}
	}
	struct upstream_user *u = cmmap_take(&uc->users, source);
	if (u != nullptr) {
		if (nick) {
			replace(&u->nickname, irc_msg_param(msg, 0));
			cmmap_set(&uc->users, u->nickname, u);
		} else {
			upstream_free_user(u);
		}
	}
	if (nick && same(uc, source, uc->nick)) {
		replace(&uc->nick, irc_msg_param(msg, 0));
		uc->regain_at = uc->now + 30000;
		upstream_notify_state(uc);
	}
	return true;
}

// ACCOUNT, CHGHOST, SETNAME, AWAY
bool upstream_handle_user_update(struct upstream *uc, struct irc_msg *msg) {
	const char *cmd = msg->command, *source = irc_msg_prefix_name(msg);
	if (!cstr_eq(cmd, "AWAY") && !need(msg, cstr_eq(cmd, "CHGHOST") ? 2 : 1)) {
		return false;
	}
	bool away = cstr_eq(cmd, "AWAY");
	struct upstream_user *u = upstream_cache_user(uc, msg->prefix, !away);
	bool own = same(uc, source, uc->nick);
	if (cstr_eq(cmd, "ACCOUNT")) {
		const char *a = cstr_eq(irc_msg_param(msg, 0), "*") ? "" : irc_msg_param(msg, 0);
		// Our own account comes from RPL_LOGGEDIN/RPL_LOGGEDOUT
		if (u != nullptr) {
			replace(&u->account, a);
		}
	} else if (cstr_eq(cmd, "CHGHOST")) {
		if (u != nullptr) {
			replace(&u->username, irc_msg_param(msg, 0));
			replace(&u->hostname, irc_msg_param(msg, 1));
		}
		if (own) {
			replace(&uc->username, irc_msg_param(msg, 0));
			replace(&uc->hostname, irc_msg_param(msg, 1));
		}
	} else if (cstr_eq(cmd, "SETNAME")) {
		if (u != nullptr) {
			replace(&u->realname, irc_msg_param(msg, 0));
		}
		if (own) {
			replace(&uc->realname, irc_msg_param(msg, 0));
		}
	} else {
		// Only if we already have the flags cached
		for (char *f = u != nullptr ? u->flags : nullptr; f != nullptr && *f != '\0'; f++) {
			if (*f == 'H' || *f == 'G') {
				*f = msg->params.len > 0 ? 'G' : 'H';
			}
		}
		if (own) {
			uc->away = msg->params.len > 0;
		}
	}
	if (own) {
		upstream_notify_state(uc);
	}
	return true;
}
