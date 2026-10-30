// PRIVMSG, NOTICE and TAGMSG (soju-rs downstream/cmds/privmsg.rs)
#include "user/cmds.h"

#include "service/service.h"

#include "base/alloc.h"
#include "base/base64.h"
#include "irc/genmsg.h"
#include "irc/numerics.h"
#include "irc/whox.h"

// soju-rs handle_privmsg
void cmd_privmsg(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		 struct user_packet *p) {
	const char *cmd = msg->command;
	bool text = !cstr_eq(cmd, "TAGMSG");
	if (!cmd_need_params(msg, text ? 2 : 1, p)) {
		return;
	}
	struct user_network *net = user_get_network(u, dc->network_id);
	strvec targets = {0};
	cmd_split_list(irc_msg_param(msg, 0), &targets);
	if (targets.len > 1) {
		cmd_drop_label(p);
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
		if (net == nullptr && cmd_same_name(CASEMAPPING_ASCII, name, dc->nick)) {
			irc_msg_free(out);
			struct irc_msg *echo = irc_msg_clone(msg);
			irc_msg_set_param(echo, 0, name);
			irc_msg_set_prefix(echo, DOWNSTREAM_SELF_PREFIX, "", "");
			user_packet_add_take(p, echo);
			continue;
		}
		if (cmd_same_name(CASEMAPPING_ASCII, name, SERVICE_NICK)) {
			if (cmd_enabled(dc, "echo-message")) {
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
		struct upstream *uc = cmd_upstream_for(net, p, cmd);
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
		    cmd_same_name(uc->casemap, target, "NickServ") &&
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
		upstream_send_labeled_take(uc, dc->id, echo ? cmd_take_label(p) : "", out);
	}
	strvec_free(&targets);
}
