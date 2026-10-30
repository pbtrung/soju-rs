// WHO, WHOIS and MONITOR, and the WHO cache (soju-rs
// downstream/cmds/query.rs)
#include "user/cmds.h"

#include "service/service.h"

#include "base/alloc.h"
#include "base/base64.h"
#include "irc/genmsg.h"
#include "irc/numerics.h"
#include "irc/whox.h"

void cmd_monitor(struct user *u, struct user_client *dc, const struct irc_msg *msg,
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

void cmd_handle_who(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		    struct user_packet *p) {
	if (msg->params.len == 0) {
		user_packet_add_take(p, IRC_MSG("315", "*", "*", "End of /WHO list"));
		return;
	}
	if (cmd_local_query(u, dc, msg, p)) {
		return;
	}
	struct upstream *uc =
		cmd_upstream_for(user_get_network(u, dc->network_id), p, msg->command);
	if (uc == nullptr) {
		return;
	}
	char *fields, *token;
	parse_whox_options(str_from(irc_msg_param(msg, 1)), &fields, &token);
	bool cached = cached_who(uc, dc, irc_msg_param(msg, 0), fields, token, p);
	xfree(fields);
	xfree(token);
	if (!cached) {
		bool queued =
			upstream_enqueue_take(uc, dc->id, cmd_take_label(p), irc_msg_clone(msg));
		(void)queued;
	}
}

bool cmd_local_query(struct user *u, struct user_client *dc, const struct irc_msg *msg,
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
