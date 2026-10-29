#include "upstream/internal.h"

#include "base/alloc.h"
#include "irc/whox.h"

static void replace(char **p, const char *s) {
	char *v = xstrdup(s);
	xfree(*p);
	*p = v;
}

static void free_member(void *v) {
	struct membership_set *ms = v;
	membership_set_free(ms);
	xfree(ms);
}

static void free_channel(void *v) {
	struct upstream_channel *ch = v;
	xfree(ch->name);
	xfree(ch->topic);
	xfree(ch->topic_who);
	xfree(ch->creation_time);
	channel_modes_free(&ch->modes);
	cmmap_free(&ch->members);
	xfree(ch);
}

static void free_user(void *v) {
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

void upstream_channels_init(struct upstream *uc) {
	cmmap_init(&uc->channels, uc->casemap, free_channel);
	cmmap_init(&uc->users, uc->casemap, free_user);
}

static struct upstream_user *cache_user(struct upstream *uc, const struct irc_prefix *prefix) {
	if (prefix == nullptr || prefix->name[0] == '\0') {
		return nullptr;
	}
	struct upstream_user *u = cmmap_get(&uc->users, prefix->name);
	if (u == nullptr) {
		u = xcalloc(1, sizeof(*u));
		u->nickname = xstrdup(prefix->name);
		u->username = xstrdup("");
		u->hostname = xstrdup("");
		u->server = xstrdup("");
		u->flags = xstrdup("H");
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

static bool same(struct upstream *uc, const char *a, const char *b) {
	AUTOFREE char *ca = casemap_cstr(uc->casemap, a);
	AUTOFREE char *cb = casemap_cstr(uc->casemap, b);
	return cstr_eq(ca, cb);
}

static struct upstream_channel *new_channel(struct upstream *uc, const char *name) {
	struct upstream_channel *ch = xcalloc(1, sizeof(*ch));
	ch->name = xstrdup(name);
	ch->topic = xstrdup("");
	ch->topic_who = xstrdup("");
	ch->creation_time = xstrdup("");
	cmmap_init(&ch->members, uc->casemap, free_member);
	cmmap_set(&uc->channels, name, ch);
	return ch;
}

static bool need(struct irc_msg *msg, size_t n) {
	return msg->params.len >= n;
}

bool upstream_handle_channel(struct upstream *uc, struct irc_msg *msg) {
	const char *cmd = msg->command, *source = irc_msg_prefix_name(msg);
	if (cstr_eq(cmd, "JOIN")) {
		if (!need(msg, 1) || source[0] == '\0') {
			return false;
		}
		struct upstream_channel *ch = cmmap_get(&uc->channels, irc_msg_param(msg, 0));
		if (same(uc, source, uc->nick)) {
			if (ch == nullptr) {
				ch = new_channel(uc, irc_msg_param(msg, 0));
			}
			if (msg->prefix != nullptr) {
				replace(&uc->username, msg->prefix->user);
				replace(&uc->hostname, msg->prefix->host);
			}
			upstream_notify_state(uc);
		}
		if (ch != nullptr) {
			struct membership_set *ms = xcalloc(1, sizeof(*ms));
			cmmap_set(&ch->members, source, ms);
		}
		struct upstream_user *u = cache_user(uc, msg->prefix);
		if (u != nullptr && need(msg, 3)) {
			replace(&u->account, irc_msg_param(msg, 1));
			replace(&u->realname, irc_msg_param(msg, 2));
		}
	} else if (cstr_eq(cmd, "352") || cstr_eq(cmd, "354")) {
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
				parse_whox_options(str_from(irc_msg_param(q->data[0].msg, 1)),
						   &fields, &token);
				valid = strchr(fields, 'n') != nullptr &&
					parse_whox_reply(msg, fields, &info, nullptr);
				xfree(fields);
				xfree(token);
			}
		}
		if (valid) {
			struct irc_prefix prefix = {.name = info.nickname,
						    .user = info.username,
						    .host = info.hostname};
			struct upstream_user *u = cache_user(uc, &prefix);
			if (u != nullptr) {
				if (info.server[0] != '\0') {
					replace(&u->server, info.server);
				}
				if (info.flags[0] != '\0') {
					replace(&u->flags, info.flags);
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
	} else if (cstr_eq(cmd, "730") || cstr_eq(cmd, "731")) {
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
				upstream_send_take(
					uc, IRC_MSG("NICK", db_get_nick(&uc->user, &uc->network)));
			}
		}
	} else if (cstr_eq(cmd, "PART") || cstr_eq(cmd, "KICK")) {
		bool kick = cstr_eq(cmd, "KICK");
		if (!need(msg, kick ? 2 : 1)) {
			return false;
		}
		const char *nick = kick ? irc_msg_param(msg, 1) : source;
		struct upstream_channel *ch = cmmap_get(&uc->channels, irc_msg_param(msg, 0));
		if (same(uc, nick, uc->nick)) {
			cmmap_del(&uc->channels, irc_msg_param(msg, 0));
		} else if (ch != nullptr) {
			cmmap_del(&ch->members, nick);
		}
	} else if (cstr_eq(cmd, "QUIT") || cstr_eq(cmd, "NICK")) {
		bool nick = cstr_eq(cmd, "NICK");
		if (source[0] == '\0' || (nick && !need(msg, 1))) {
			return false;
		}
		void *v;
		for (size_t it = 0; cmmap_next(&uc->channels, &it, nullptr, &v);) {
			struct upstream_channel *ch = v;
			struct membership_set *ms = cmmap_take(&ch->members, source);
			if (ms != nullptr) {
				if (nick) {
					cmmap_set(&ch->members, irc_msg_param(msg, 0), ms);
				} else {
					free_member(ms);
				}
			}
		}
		struct upstream_user *u = cmmap_take(&uc->users, source);
		if (u != nullptr) {
			if (nick) {
				replace(&u->nickname, irc_msg_param(msg, 0));
				cmmap_set(&uc->users, u->nickname, u);
			} else {
				free_user(u);
			}
		}
		if (nick && same(uc, source, uc->nick)) {
			replace(&uc->nick, irc_msg_param(msg, 0));
			uc->regain_at = uc->now + 30000;
			upstream_notify_state(uc);
		}
	} else if (cstr_eq(cmd, "353")) {
		if (!need(msg, 4)) {
			return false;
		}
		struct upstream_channel *ch = cmmap_get(&uc->channels, irc_msg_param(msg, 2));
		if (ch == nullptr) {
			return true;
		}
		enum channel_status status;
		if (channel_status_parse(str_from(irc_msg_param(msg, 1)), &status, nullptr)) {
			ch->status = status;
		}
		struct str rest = str_from(irc_msg_param(msg, 3)), part;
		while (str_next_field(&rest, ' ', &part)) {
			if (part.len == 0) {
				continue;
			}
			struct membership_set *ms = xcalloc(1, sizeof(*ms));
			bool found = true;
			while (part.len > 0 && found) {
				found = false;
				for (size_t i = 0; i < uc->memberships.len; i++) {
					if ((uint8_t)part.p[0] == uc->memberships.data[i].prefix) {
						membership_set_add(ms, uc->memberships.data,
								   uc->memberships.len,
								   uc->memberships.data[i]);
						part = str_slice(part, 1, part.len);
						found = true;
						break;
					}
				}
			}
			struct str name, user, host;
			irc_prefix_split(part, &name, &user, &host);
			AUTOFREE char *n = str_dup(name);
			if (n[0] != '\0') {
				cmmap_set(&ch->members, n, ms);
			} else {
				free_member(ms);
			}
		}
	} else if (cstr_eq(cmd, "366")) {
		if (!need(msg, 2)) {
			return false;
		}
		struct upstream_channel *ch = cmmap_get(&uc->channels, irc_msg_param(msg, 1));
		if (ch != nullptr) {
			ch->complete = true;
			upstream_notify_state(uc);
		}
	} else if (cstr_eq(cmd, "TOPIC") || cstr_eq(cmd, "332") || cstr_eq(cmd, "331") ||
		   cstr_eq(cmd, "333")) {
		bool topic = cstr_eq(cmd, "TOPIC"), who = cstr_eq(cmd, "333");
		if (!need(msg, who ? 4 : topic ? 2 : 3)) {
			return false;
		}
		struct upstream_channel *ch =
			cmmap_get(&uc->channels, irc_msg_param(msg, topic ? 0 : 1));
		if (ch != nullptr) {
			if (who) {
				replace(&ch->topic_who, irc_msg_param(msg, 2));
				long long t;
				if (str_parse_i64(str_from(irc_msg_param(msg, 3)), &t)) {
					ch->topic_time =
						(struct db_time){.set = true, .t = {.sec = t}};
				}
			} else {
				replace(&ch->topic, cstr_eq(cmd, "331")
							    ? ""
							    : irc_msg_param(msg, topic ? 1 : 2));
				if (topic) {
					AUTOFREE char *prefix =
						msg->prefix != nullptr
							? xasprintf("%s!%s@%s", source,
								    msg->prefix->user,
								    msg->prefix->host)
							: xstrdup("");
					replace(&ch->topic_who, prefix);
					ch->topic_time =
						(struct db_time){.set = true, .t = timestamp_now()};
				}
			}
		}
	} else if (cstr_eq(cmd, "MODE") || cstr_eq(cmd, "324")) {
		bool numeric = cstr_eq(cmd, "324");
		size_t target = numeric ? 1 : 0;
		if (!need(msg, target + 2)) {
			return false;
		}
		const char *name = irc_msg_param(msg, target);
		struct upstream_channel *ch = cmmap_get(&uc->channels, name);
		struct err *e = nullptr;
		if (ch != nullptr) {
			if (numeric) {
				channel_modes_free(&ch->modes);
			}
			bool applied = apply_channel_modes(
				&ch->modes, &ch->members, uc->memberships.data, uc->memberships.len,
				&uc->mode_types, str_from(irc_msg_param(msg, target + 1)),
				(const char *const *)&msg->params.data[target + 2],
				msg->params.len - target - 2, &e);
			(void)applied;
		} else if (same(uc, name, uc->nick)) {
			bool applied = user_modes_apply(
				&uc->modes, str_from(irc_msg_param(msg, target + 1)), &e);
			(void)applied;
		}
		err_free(e);
	} else if (cstr_eq(cmd, "ACCOUNT") || cstr_eq(cmd, "CHGHOST") || cstr_eq(cmd, "SETNAME") ||
		   cstr_eq(cmd, "AWAY")) {
		if (!cstr_eq(cmd, "AWAY") && !need(msg, cstr_eq(cmd, "CHGHOST") ? 2 : 1)) {
			return false;
		}
		struct upstream_user *u = cache_user(uc, msg->prefix);
		bool own = same(uc, source, uc->nick);
		if (cstr_eq(cmd, "ACCOUNT")) {
			const char *a =
				cstr_eq(irc_msg_param(msg, 0), "*") ? "" : irc_msg_param(msg, 0);
			if (u != nullptr) {
				replace(&u->account, a);
			}
			if (own) {
				replace(&uc->account, a);
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
			if (u != nullptr) {
				replace(&u->flags, msg->params.len > 0 ? "G" : "H");
			}
			if (own) {
				uc->away = msg->params.len > 0;
			}
		}
		if (own) {
			upstream_notify_state(uc);
		}
	} else if (cstr_eq(cmd, "PRIVMSG") || cstr_eq(cmd, "NOTICE") || cstr_eq(cmd, "TAGMSG")) {
		if (!need(msg, cstr_eq(cmd, "TAGMSG") ? 1 : 2)) {
			return false;
		}
		(void)cache_user(uc, msg->prefix);
		const char *target = irc_msg_param(msg, 0);
		while (*target != '\0' && strchr(uc->status_msg, *target) != nullptr) {
			target++;
		}
		struct upstream_channel *ch = cmmap_get(&uc->channels, target);
		if (ch != nullptr && ch->detach_after_ms > 0) {
			ch->detach_deadline = uc->now + ch->detach_after_ms;
		}
	}
	return true;
}
