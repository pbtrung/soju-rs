// Channels' state: membership, topics, modes and names (soju-rs
// upstream/channels.rs)
#include "upstream/state.h"

#include "base/alloc.h"
#include "irc/whox.h"

void upstream_free_member(void *v) {
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

void upstream_channels_init(struct upstream *uc) {
	cmmap_init(&uc->channels, uc->casemap, free_channel);
	cmmap_init(&uc->users, uc->casemap, upstream_free_user);
}

static struct upstream_channel *new_channel(struct upstream *uc, const char *name) {
	struct upstream_channel *ch = xcalloc(1, sizeof(*ch));
	ch->name = xstrdup(name);
	ch->topic = xstrdup("");
	ch->topic_who = xstrdup("");
	ch->creation_time = xstrdup("");
	cmmap_init(&ch->members, uc->casemap, upstream_free_member);
	cmmap_set(&uc->channels, name, ch);
	return ch;
}

// Forgets a channel we left, and the users we don't share a channel with
// anymore
static void forget_channel(struct upstream *uc, const char *name) {
	struct upstream_channel *ch = cmmap_take(&uc->channels, name);
	if (ch == nullptr) {
		return;
	}
	const char *nick;
	for (size_t it = 0; cmmap_next(&ch->members, &it, &nick, nullptr);) {
		upstream_forget_user(uc, nick);
	}
	free_channel(ch);
}

// JOIN
static bool handle_join(struct upstream *uc, struct irc_msg *msg) {
	const char *source = irc_msg_prefix_name(msg);
	if (!need(msg, 1) || source[0] == '\0') {
		return false;
	}
	struct upstream_channel *ch = cmmap_get(&uc->channels, irc_msg_param(msg, 0));
	if (same(uc, source, uc->nick)) {
		// A JOIN of a channel we're in starts it over
		forget_channel(uc, irc_msg_param(msg, 0));
		ch = new_channel(uc, irc_msg_param(msg, 0));
		upstream_send_take(uc, IRC_MSG("MODE", ch->name));
		upstream_notify_state(uc);
	}
	if (ch != nullptr) {
		struct membership_set *ms = xcalloc(1, sizeof(*ms));
		cmmap_set(&ch->members, source, ms);
	}
	struct upstream_user *u = upstream_cache_user(uc, msg->prefix, true);
	if (u != nullptr && cap_registry_is_enabled(&uc->caps, "away-notify")) {
		// The H/G flag is updated by AWAY messages, and B (bot mode)
		// can't be tracked after the JOIN
		const char *bot = strmap_get(&uc->isupport.m, "BOT");
		AUTOFREE char *flags =
			xasprintf("H%s", irc_tags_contains(&msg->tags, "bot") && bot != nullptr &&
							 strlen(bot) == 1
						 ? bot
						 : "");
		replace(&u->flags, flags);
	}
	if (u != nullptr && need(msg, 3)) {
		replace(&u->account, irc_msg_param(msg, 1));
		replace(&u->realname, irc_msg_param(msg, 2));
	}
	return true;
}

// PART, KICK
static bool handle_part(struct upstream *uc, struct irc_msg *msg) {
	const char *cmd = msg->command, *source = irc_msg_prefix_name(msg);
	bool kick = cstr_eq(cmd, "KICK");
	if (!need(msg, kick ? 2 : 1)) {
		return false;
	}
	AUTOFREE char *nick = xstrdup(kick ? irc_msg_param(msg, 1) : source);
	struct upstream_channel *ch = cmmap_get(&uc->channels, irc_msg_param(msg, 0));
	if (same(uc, nick, uc->nick)) {
		forget_channel(uc, irc_msg_param(msg, 0));
	} else if (ch != nullptr) {
		cmmap_del(&ch->members, nick);
		upstream_forget_user(uc, nick);
	}
	return true;
}

// 353
static bool handle_names_reply(struct upstream *uc, struct irc_msg *msg) {
	if (!need(msg, 4)) {
		return false;
	}
	struct upstream_channel *ch = cmmap_get(&uc->channels, irc_msg_param(msg, 2));
	if (ch == nullptr) {
		// NAMES on a channel we have not joined, forward to downstream
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
			upstream_free_member(ms);
		}
	}
	// A joined channel's members: sent with its state on RPL_ENDOFNAMES
	return false;
}

// 366
static bool handle_end_of_names(struct upstream *uc, struct irc_msg *msg) {
	if (!need(msg, 2)) {
		return false;
	}
	struct upstream_channel *ch = cmmap_get(&uc->channels, irc_msg_param(msg, 1));
	if (ch == nullptr) {
		// NAMES on a channel we have not joined, forward to downstream
		return true;
	}
	// The channel's state is sent to downstreams instead (soju-rs
	// forward_channel)
	if (ch->complete) {
		log_info(&uc->log, "received unexpected RPL_ENDOFNAMES");
		return false;
	}
	ch->complete = true;
	upstream_notify_state(uc);
	if (uc->events.channel_complete != nullptr) {
		uc->events.channel_complete(uc, ch->name, uc->events.data);
	}
	return false;
}

// TOPIC, 332, 331, 333
static bool handle_topic(struct upstream *uc, struct irc_msg *msg) {
	const char *cmd = msg->command, *source = irc_msg_prefix_name(msg);
	bool topic = cstr_eq(cmd, "TOPIC"), who = cstr_eq(cmd, "333");
	if (!need(msg, who ? 4 : topic ? 2 : 3)) {
		return false;
	}
	struct upstream_channel *ch = cmmap_get(&uc->channels, irc_msg_param(msg, topic ? 0 : 1));
	// The topic of a channel we're in is sent with its state, or
	// to the clients asking for it (soju-rs only forwards the first
	// RPL_TOPICWHOTIME)
	bool forward = topic || ch == nullptr || (who && ch->topic_who[0] == '\0');
	if (ch != nullptr) {
		if (who) {
			replace(&ch->topic_who, irc_msg_param(msg, 2));
			long long t;
			if (str_parse_i64(str_from(irc_msg_param(msg, 3)), &t)) {
				ch->topic_time = (struct db_time){.set = true, .t = {.sec = t}};
			}
		} else {
			replace(&ch->topic,
				cstr_eq(cmd, "331") ? "" : irc_msg_param(msg, topic ? 1 : 2));
			if (topic) {
				AUTOFREE char *prefix =
					msg->prefix != nullptr
						? xasprintf("%s!%s@%s", source, msg->prefix->user,
							    msg->prefix->host)
						: xstrdup("");
				replace(&ch->topic_who, prefix);
				struct timestamp t;
				if (!parse_server_time(
					    str_from(irc_tags_get_or_empty(&msg->tags, "time")),
					    &t)) {
					t = timestamp_now();
				}
				ch->topic_time = (struct db_time){.set = true, .t = t};
			}
		}
	}
	return forward;
}

// 329
static bool handle_creation_time(struct upstream *uc, struct irc_msg *msg) {
	if (!need(msg, 3)) {
		return false;
	}
	struct upstream_channel *ch = cmmap_get(&uc->channels, irc_msg_param(msg, 1));
	if (ch == nullptr) {
		return true;
	}
	// Only the first one is forwarded, the clients asking get it
	// from us
	bool first = ch->creation_time[0] == '\0';
	replace(&ch->creation_time, irc_msg_param(msg, 2));
	return first;
}

// MODE, 324, 221
static bool handle_mode(struct upstream *uc, struct irc_msg *msg) {
	const char *cmd = msg->command;
	bool numeric = cstr_eq(cmd, "324");
	size_t target = numeric ? 1 : 0;
	if (!need(msg, target + 2)) {
		return false;
	}
	const char *name = irc_msg_param(msg, target);
	struct upstream_channel *ch = cmmap_get(&uc->channels, name);
	struct err *e = nullptr;
	bool forward = true;
	if (ch != nullptr) {
		if (numeric) {
			channel_modes_free(&ch->modes);
			forward = !ch->has_modes;
			ch->has_modes = true;
		}
		bool applied = apply_channel_modes(
			&ch->modes, &ch->members, uc->memberships.data, uc->memberships.len,
			&uc->mode_types, str_from(irc_msg_param(msg, target + 1)),
			(const char *const *)&msg->params.data[target + 2],
			msg->params.len - target - 2, &e);
		(void)applied;
	} else if (same(uc, name, uc->nick)) {
		if (cstr_eq(cmd, "221")) {
			user_modes_free(&uc->modes);
		}
		bool applied =
			user_modes_apply(&uc->modes, str_from(irc_msg_param(msg, target + 1)), &e);
		(void)applied;
	}
	err_free(e);
	return forward;
}

// PRIVMSG, NOTICE, TAGMSG
static bool handle_message(struct upstream *uc, struct irc_msg *msg) {
	const char *cmd = msg->command;
	if (!need(msg, cstr_eq(cmd, "TAGMSG") ? 1 : 2)) {
		return false;
	}
	const char *target = irc_msg_param(msg, 0);
	while (*target != '\0' && strchr(uc->status_msg, *target) != nullptr) {
		target++;
	}
	struct upstream_channel *ch = cmmap_get(&uc->channels, target);
	if (ch != nullptr && ch->detach_after_ms > 0) {
		ch->detach_deadline = uc->now + ch->detach_after_ms;
	}
	return true;
}

// The handlers of the messages which change channel or user state. They
// return false for a message not to forward.
static const struct {
	const char *command;
	bool (*handle)(struct upstream *uc, struct irc_msg *msg);
} handlers[] = {
	{"JOIN", handle_join},
	{"352", upstream_handle_who_reply},
	{"354", upstream_handle_who_reply},
	{"730", upstream_handle_monitor},
	{"731", upstream_handle_monitor},
	{"PART", handle_part},
	{"KICK", handle_part},
	{"QUIT", upstream_handle_quit_nick},
	{"NICK", upstream_handle_quit_nick},
	{"353", handle_names_reply},
	{"366", handle_end_of_names},
	{"TOPIC", handle_topic},
	{"332", handle_topic},
	{"331", handle_topic},
	{"333", handle_topic},
	{"329", handle_creation_time},
	{"MODE", handle_mode},
	{"324", handle_mode},
	{"221", handle_mode},
	{"ACCOUNT", upstream_handle_user_update},
	{"CHGHOST", upstream_handle_user_update},
	{"SETNAME", upstream_handle_user_update},
	{"AWAY", upstream_handle_user_update},
	{"PRIVMSG", handle_message},
	{"NOTICE", handle_message},
	{"TAGMSG", handle_message},
};

bool upstream_handle_channel(struct upstream *uc, struct irc_msg *msg) {
	for (size_t i = 0; i < sizeof(handlers) / sizeof(*handlers); i++) {
		if (cstr_eq(msg->command, handlers[i].command)) {
			return handlers[i].handle(uc, msg);
		}
	}
	return true;
}
