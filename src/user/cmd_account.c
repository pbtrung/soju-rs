// NICK, SETNAME and post-registration AUTHENTICATE (soju-rs
// downstream/cmds/basic.rs, account.rs)
#include "user/cmds.h"

#include "service/service.h"

#include "base/alloc.h"
#include "base/base64.h"
#include "irc/genmsg.h"
#include "irc/numerics.h"
#include "irc/whox.h"

void cmd_handle_nick(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		     struct user_packet *p) {
	if (!cmd_need_params(msg, 1, p)) {
		return;
	}
	const char *nick = irc_msg_param(msg, 0);
	struct user_network *net = user_get_network(u, dc->network_id);
	if (nick[0] == '\0' || strpbrk(nick, ILLEGAL_NICK_CHARS) != nullptr) {
		user_packet_add_take(
			p, IRC_MSG("432", dc->nick, nick, "Nickname contains illegal characters"));
		return;
	}
	if (cmd_same_name(cmd_client_casemap(net), nick, SERVICE_NICK)) {
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
		cmd_sync_bouncer_clients(u, true);
	} else if (net->upstream != nullptr && net->upstream->attached) {
		cmd_send_labeled(net->upstream, dc, p, IRC_MSG("NICK", nick));
	} else {
		user_packet_sync(u, net, p, false);
	}
}

void cmd_handle_setname(struct user *u, struct user_client *dc, const struct irc_msg *msg,
			struct user_packet *p) {
	if (!cmd_need_params(msg, 1, p)) {
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
			cmd_send_labeled(uc, dc, p, IRC_MSG("SETNAME", realname));
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
		cmd_sync_bouncer_clients(u, false);
	}
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
void cmd_authenticate(struct user_client *dc, struct user_network *net, const struct irc_msg *msg,
		      struct user_packet *p) {
	struct upstream *uc = net->upstream;
	if (uc == nullptr || !uc->attached || !cap_registry_is_enabled(&uc->caps, "sasl")) {
		dc->sasl_plain = false;
		user_packet_add_take(p, IRC_MSG("904", dc->nick,
						"Upstream network authentication not supported"));
		return;
	}
	if (!cmd_need_params(msg, 1, p)) {
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
		bool queued = upstream_enqueue_take(uc, dc->id, cmd_take_label(p),
						    IRC_MSG("AUTHENTICATE", "PLAIN"));
		(void)queued;
		cmd_no_ack(p);
	}
	wipe(&decoded);
}
