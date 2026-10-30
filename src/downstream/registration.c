// Unregistered commands and registration (soju-rs
// downstream/registration.rs)
#include "downstream/internal.h"

#include <openssl/evp.h>
#include <stdio.h>
#include <string.h>

#include "auth/auth.h"
#include "base/alloc.h"
#include "base/base64.h"
#include "base/utf8.h"
#include "db/db.h"
#include "irc/genmsg.h"
#include "irc/ircutil.h"
#include "irc/numerics.h"
#include "net/url.h"
#include "user/user.h"

struct result downstream_handle_unregistered(struct downstream *dc, const struct irc_msg *msg) {
	const char *cmd = msg->command;
	struct registration *r = dc->reg;
	if (cstr_eq(cmd, "NICK")) {
		if (!need_params(msg, 1)) {
			return need_more_params(cmd);
		}
		replace_str(&r->nick, msg->params.data[0]);
	} else if (cstr_eq(cmd, "USER")) {
		if (!need_params(msg, 4)) {
			return need_more_params(cmd);
		}
		replace_str(&r->username, msg->params.data[0]);
	} else if (cstr_eq(cmd, "PASS")) {
		if (!need_params(msg, 1)) {
			return need_more_params(cmd);
		}
		replace_str(&r->pass, msg->params.data[0]);
	} else if (cstr_eq(cmd, "CAP")) {
		return downstream_handle_cap(dc, msg);
	} else if (cstr_eq(cmd, "AUTHENTICATE")) {
		bool complete;
		struct result res = downstream_handle_authenticate(dc, msg, &complete);
		if (res.irc != nullptr || res.fatal != nullptr) {
			downstream_sasl_free(dc->sasl);
			dc->sasl = nullptr;
			return res;
		}
		if (complete) {
			downstream_authenticate(dc, dc->sasl);
		}
	} else if (cstr_eq(cmd, "BOUNCER")) {
		if (!need_params(msg, 1)) {
			return need_more_params(cmd);
		}
		const char *sub = msg->params.data[0];
		AUTO_STRBUF struct strbuf up = STRBUF_INIT;
		utf8_to_upper(&up, str_from(sub));
		if (!cstr_eq(strbuf_cstr(&up), "BIND")) {
			return irc_err(IRC_MSG("FAIL", "BOUNCER", "UNKNOWN_COMMAND", sub,
					       "Unknown subcommand"));
		}
		if (!need_params(msg, 2)) {
			return need_more_params(cmd);
		}
		if (r->auth_username[0] == '\0') {
			return irc_err(IRC_MSG("FAIL", "BOUNCER", "ACCOUNT_REQUIRED", "BIND",
					       "Authentication needed to bind to bouncer network"));
		}
		long long id;
		const char *s = msg->params.data[1];
		if (!str_parse_i64(str_from(s), &id) || id <= 0) {
			return irc_err(IRC_MSG("FAIL", "BOUNCER", "INVALID_NETID", sub, s,
					       "Invalid network ID"));
		}
		r->network_id = id;
	} else if (cstr_eq(cmd, "AWAY")) {
		xfree(dc->away);
		dc->away = msg->params.len > 0 ? xstrdup(msg->params.data[0]) : nullptr;
		downstream_send(dc, generate_away_reply(dc->away != nullptr));
	} else if (cstr_eq(cmd, "METADATA")) {
		if (!need_params(msg, 2)) {
			return need_more_params(cmd);
		}
		const char *sub = msg->params.data[1];
		if (!downstream_handle_metadata_sub(dc, msg)) {
			return irc_err(IRC_MSG("FAIL", "METADATA", "INVALID_PARAMS", sub,
					       "Unknown command"));
		}
	} else {
		log_debug(&dc->log, "unhandled message: %s", cmd);
		return irc_err(IRC_MSG(ERR_UNKNOWNCOMMAND, "*", cmd, "Unknown command"));
	}
	return ok();
}

// After PASS: authenticated asynchronously
struct pass_job {
	uint64_t dc_id;
	char *reg_username;
	char *username, *client, *network;
};

static void pass_job_free(struct pass_job *j) {
	xfree(j->reg_username);
	xfree(j->username);
	xfree(j->client);
	xfree(j->network);
	xfree(j);
}

void downstream_reply_and_close(struct downstream *dc, struct irc_msg *m) {
	server_set_prefix(dc->srv, m);
	downstream_send(dc, m);
	downstream_close(dc);
}

static void finish_registration(struct downstream *dc);

static void pass_done(void *data, const struct auth_error *err) {
	struct pass_job *j = data;
	struct downstream *dc = downstream_lookup(j->dc_id);
	if (dc == nullptr) {
		pass_job_free(j);
		return;
	}
	dc->busy = false;
	if (err != nullptr) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(j->reg_username));
		log_info(&dc->log, "PASS authentication error for user %s: %s", q, err->internal);
		downstream_reply_and_close(dc,
					   IRC_MSG(ERR_PASSWDMISMATCH, dc->nick, err->external));
		pass_job_free(j);
		return;
	}
	downstream_set_auth_username(dc, j->username, j->client, j->network);
	pass_job_free(j);
	finish_registration(dc);
	downstream_drain_input(dc);
}

// soju-rs register(): authentication by PASS if SASL wasn't used, then
// the client and network names
void downstream_try_register(struct downstream *dc) {
	if (dc->sasl != nullptr) {
		downstream_end_sasl(
			dc, IRC_MSG(ERR_SASLABORTED, dc->nick, "SASL authentication aborted"));
	}
	struct registration *r = dc->reg;
	char *password = r->pass;
	r->pass = xstrdup("");
	if (r->auth_username[0] != '\0') {
		explicit_bzero(password, strlen(password));
		xfree(password);
		finish_registration(dc);
		return;
	}
	if (password[0] == '\0') {
		xfree(password);
		if (cap_registry_is_enabled(&dc->caps, "sasl")) {
			downstream_reply_and_close(dc, IRC_MSG("FAIL", "*", "ACCOUNT_REQUIRED",
							       "Authentication required"));
		} else {
			downstream_reply_and_close(dc, IRC_MSG(ERR_PASSWDMISMATCH, dc->nick,
							       "Authentication required"));
		}
		return;
	}
	struct server_config *cfg = server_config_get(dc->srv);
	bool plain = cfg->plain_auth != PLAIN_AUTH_NONE;
	server_config_unref(cfg);
	if (!plain) {
		explicit_bzero(password, strlen(password));
		xfree(password);
		downstream_reply_and_close(
			dc, IRC_MSG(ERR_PASSWDMISMATCH, dc->nick, "PASS authentication disabled"));
		return;
	}
	struct pass_job *j = xcalloc(1, sizeof(*j));
	j->dc_id = dc->id;
	j->reg_username = xstrdup(r->username);
	unmarshal_username(r->username, &j->username, &j->client, &j->network);
	dc->busy = true;
	auth_plain(dc->srv, j->username, password, dc->remote_addr, pass_done, j);
	explicit_bzero(password, strlen(password));
	xfree(password);
}

static void finish_registration(struct downstream *dc) {
	struct registration *r = dc->reg;
	AUTOFREE char *u = nullptr;
	AUTOFREE char *fallback_client = nullptr;
	AUTOFREE char *fallback_network = nullptr;
	unmarshal_username(r->username, &u, &fallback_client, &fallback_network);
	if (dc->client_name[0] == '\0') {
		replace_str(&dc->client_name, fallback_client);
	} else if (fallback_client[0] != '\0' && !cstr_eq(dc->client_name, fallback_client)) {
		downstream_reply_and_close(dc, IRC_MSG(ERR_ERRONEUSNICKNAME, "*",
						       "Client name mismatch in usernames"));
		return;
	}
	if (r->network_name[0] == '\0') {
		replace_str(&r->network_name, fallback_network);
	} else if (fallback_network[0] != '\0' && !cstr_eq(r->network_name, fallback_network)) {
		downstream_reply_and_close(dc, IRC_MSG(ERR_ERRONEUSNICKNAME, "*",
						       "Network name mismatch in usernames"));
		return;
	}
	if (cstr_eq(r->network_name, "*")) {
		downstream_reply_and_close(dc,
					   IRC_MSG(ERR_PASSWDMISMATCH, "*",
						   "Multi-upstream mode is no longer supported"));
		return;
	}
	dc->registered = true;
	replace_str(&dc->username, r->auth_username);
	AUTOFREE char *q = utf8_debug_cstr(str_from(dc->username));
	log_info(&dc->log, "registration complete for user %s", q);
	uv_timer_stop(&dc->register_timer);
	if (dc->events.registered != nullptr) {
		dc->events.registered(dc, dc->events.data);
	} else {
		downstream_close(dc);
	}
}
