#include "upstream/internal.h"

#include "base/alloc.h"
#include "base/base64.h"
#include "base/utf8.h"
#include "irc/genmsg.h"
#include "irc/numerics.h"

static const char *const permanent_caps[] = {"account-notify",
					     "account-tag",
					     "away-notify",
					     "batch",
					     "chghost",
					     "extended-join",
					     "extended-monitor",
					     "invite-notify",
					     "labeled-response",
					     "message-tags",
					     "multi-prefix",
					     "sasl",
					     "server-time",
					     "setname",
					     "draft/account-registration",
					     "draft/extended-monitor",
					     "draft/message-redaction",
					     "draft/multiline"};

static void replace(char **p, const char *s) {
	char *copy = xstrdup(s);
	xfree(*p);
	*p = copy;
}

void upstream_caps_to_request(const struct cap_registry *caps, strvec *out) {
	for (size_t i = 0; i < sizeof(permanent_caps) / sizeof(*permanent_caps); i++) {
		const char *c = permanent_caps[i];
		if (cap_registry_is_available(caps, c) && !cap_registry_is_enabled(caps, c)) {
			strvec_push(out, c);
		}
	}
	bool want_echo = cap_registry_is_available(caps, "labeled-response") &&
			 cap_registry_is_available(caps, "echo-message");
	if (want_echo && !cap_registry_is_enabled(caps, "echo-message")) {
		strvec_push(out, "echo-message");
	} else if (!want_echo && cap_registry_is_enabled(caps, "echo-message")) {
		strvec_push(out, "-echo-message");
	}
}

static void request_caps(struct upstream *uc) {
	strvec caps = {0};
	upstream_caps_to_request(&uc->caps, &caps);
	AUTO_STRBUF struct strbuf text = STRBUF_INIT;
	for (size_t i = 0; i < caps.len; i++) {
		if (text.len > 0) {
			strbuf_append_char(&text, ' ');
		}
		strbuf_append_cstr(&text, caps.data[i]);
	}
	if (text.len > 0) {
		upstream_send_take(uc, IRC_MSG("CAP", "REQ", strbuf_cstr(&text)));
	}
	strvec_free(&caps);
}

void upstream_register(struct upstream *uc) {
	uc->state = UPSTREAM_REGISTERING;
	uc->deadline = uc->now + UPSTREAM_REGISTER_TIMEOUT_MS;
	uc->rate_enabled = true;
	upstream_send_take(uc, IRC_MSG("CAP", "LS", "302"));
	if (uc->network.pass[0] != '\0') {
		upstream_send_take(uc, IRC_MSG("PASS", uc->network.pass));
	}
	upstream_send_take(uc, IRC_MSG("NICK", uc->nick));
	upstream_send_take(uc, IRC_MSG("USER", uc->username, "0", "*", uc->realname));
}

static bool wants_sasl(struct upstream *uc) {
	return uc->network.sasl.mechanism[0] != '\0' &&
	       upstream_supports_sasl(uc, uc->network.sasl.mechanism);
}

static void supported_caps(struct upstream *uc, const char *s) {
	struct str rest = str_from(s), part;
	while (str_next_field(&rest, ' ', &part)) {
		if (part.len == 0) {
			continue;
		}
		struct str name, value;
		if (!str_split_once(part, '=', &name, &value)) {
			name = part;
			value = STR_EMPTY;
		}
		AUTOFREE char *k = casemap(CASEMAPPING_ASCII, name);
		AUTOFREE char *v = str_dup(value);
		cap_registry_set_available(&uc->caps, k, v);
	}
}

static void handle_cap(struct upstream *uc, struct irc_msg *msg) {
	if (msg->params.len < 3) {
		return;
	}
	const char *sub = irc_msg_param(msg, 1), *caps = irc_msg_trailing(msg);
	if (cstr_eq(sub, "LS") || cstr_eq(sub, "NEW")) {
		supported_caps(uc, caps);
		if (cstr_eq(sub, "LS") && msg->params.len >= 4 &&
		    cstr_eq(irc_msg_param(msg, msg->params.len - 2), "*")) {
			return;
		}
		request_caps(uc);
		if (cstr_eq(sub, "LS") && !wants_sasl(uc)) {
			upstream_send_take(uc, IRC_MSG("CAP", "END"));
		}
	} else if (cstr_eq(sub, "ACK") || cstr_eq(sub, "NAK") || cstr_eq(sub, "DEL")) {
		struct str rest = str_from(caps), part;
		while (str_next_field(&rest, ' ', &part)) {
			if (part.len == 0) {
				continue;
			}
			bool enabled = cstr_eq(sub, "ACK");
			if (part.p[0] == '-') {
				enabled = false;
				part = str_slice(part, 1, part.len);
			}
			AUTOFREE char *name = casemap(CASEMAPPING_ASCII, part);
			if (cstr_eq(sub, "DEL")) {
				cap_registry_del(&uc->caps, name);
			} else {
				cap_registry_set_enabled(&uc->caps, name, enabled);
			}
			if (cstr_eq(name, "sasl") && wants_sasl(uc)) {
				if (enabled) {
					uc->sasl_active = true;
					uc->sasl_started = false;
					upstream_send_take(uc, IRC_MSG("AUTHENTICATE",
								       uc->network.sasl.mechanism));
				} else if (!uc->registered) {
					upstream_send_take(uc, IRC_MSG("CAP", "END"));
				}
			}
		}
		upstream_notify_state(uc);
	}
}

static void handle_isupport(struct upstream *uc, const struct irc_msg *msg) {
	for (size_t i = 1; i + 1 < msg->params.len; i++) {
		struct str token = str_from(msg->params.data[i]), name, value;
		bool negate = token.len > 0 && token.p[0] == '-';
		if (negate) {
			token = str_slice(token, 1, token.len);
		}
		bool has_value = str_split_once(token, '=', &name, &value);
		if (!has_value) {
			name = token;
			value = STR_EMPTY;
		}
		AUTOFREE char *key = str_dup(name);
		for (size_t j = 0; key[j] != '\0'; j++) {
			if (key[j] >= 'a' && key[j] <= 'z') {
				key[j] = (char)(key[j] - 'a' + 'A');
			}
		}
		if (strchr(key, '/') != nullptr) {
			for (size_t j = 0; key[j] != '/'; j++) {
				if (key[j] >= 'A' && key[j] <= 'Z') {
					key[j] = (char)(key[j] - 'A' + 'a');
				}
			}
		}
		AUTO_STRBUF struct strbuf decoded = STRBUF_INIT;
		decode_isupport_value(&decoded, value);
		const char *v = strbuf_cstr(&decoded);
		if (negate) {
			strmap_remove(&uc->isupport.m, key);
		} else {
			isupport_set(&uc->isupport, key, has_value ? v : nullptr);
		}
		if (cstr_eq(key, "CASEMAPPING")) {
			enum casemapping cm = CASEMAPPING_RFC1459;
			bool parsed = parse_casemapping(str_from(v), &cm);
			(void)parsed;
			upstream_set_casemapping(uc, cm);
		} else if (cstr_eq(key, "CHANTYPES")) {
			replace(&uc->channel_types, negate ? STD_CHANNEL_TYPES : v);
		} else if (cstr_eq(key, "STATUSMSG")) {
			replace(&uc->status_msg, negate ? "" : v);
		} else if (cstr_eq(key, "soju.im/SAFERATE")) {
			uc->rate_enabled = negate;
		} else if (cstr_eq(key, "CHANMODES")) {
			struct channel_mode_types modes = {0};
			struct str rest = str_from(v), field;
			bool valid = true;
			for (unsigned j = 0; j < 4; j++) {
				if (!str_next_field(&rest, ',', &field)) {
					valid = false;
					break;
				}
				for (size_t k = 0; k < field.len; k++) {
					channel_mode_types_set(&modes, (uint8_t)field.p[k],
							       (enum channel_mode_type)j);
				}
			}
			if (negate) {
				channel_mode_types_free(&modes);
				std_channel_modes(&modes);
				valid = true;
			}
			if (valid) {
				channel_mode_types_free(&uc->mode_types);
				uc->mode_types = modes;
			} else {
				channel_mode_types_free(&modes);
			}
		} else if (cstr_eq(key, "PREFIX")) {
			if (negate) {
				vec_clear(&uc->memberships);
				size_t n;
				const struct membership *ms = std_memberships(&n);
				for (size_t j = 0; j < n; j++) {
					vec_push(&uc->memberships, ms[j]);
				}
			} else if (v[0] == '\0') {
				vec_clear(&uc->memberships);
			} else {
				const char *sep = strchr(v, ')');
				if (v[0] == '(' && sep != nullptr &&
				    strlen(sep + 1) == (size_t)(sep - v - 1) &&
				    strspn(v,
					   "()abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ~"
					   "&@%+!.-0123456789") == strlen(v)) {
					vec_clear(&uc->memberships);
					for (size_t j = 0; j < strlen(sep + 1); j++) {
						vec_push(&uc->memberships,
							 ((struct membership){
								 .mode = (uint8_t)v[j + 1],
								 .prefix = (uint8_t)sep[j + 1]}));
					}
				}
			}
		}
	}
	upstream_notify_state(uc);
}

bool upstream_handle_registration(struct upstream *uc, struct irc_msg *msg) {
	const char *cmd = msg->command;
	if (cstr_eq(cmd, "CAP")) {
		handle_cap(uc, msg);
	} else if (cstr_eq(cmd, "AUTHENTICATE")) {
		if (!uc->sasl_active || uc->sasl_started || msg->params.len == 0) {
			upstream_send_take(uc, IRC_MSG("AUTHENTICATE", "*"));
			return true;
		}
		AUTO_STRBUF struct strbuf challenge = STRBUF_INIT;
		const char *v = irc_msg_param(msg, 0);
		if (!cstr_eq(v, "+") &&
		    !base64_decode(&challenge, BASE64_STANDARD, str_from(v), nullptr)) {
			upstream_send_take(uc, IRC_MSG("AUTHENTICATE", "*"));
			return true;
		}
		AUTO_STRBUF struct strbuf response = STRBUF_INIT;
		if (uc->sasl_username != nullptr) {
			// Post-registration, with a downstream's credentials
			strbuf_append_char(&response, '\0');
			strbuf_append_cstr(&response, uc->sasl_username);
			strbuf_append_char(&response, '\0');
			strbuf_append_cstr(&response, uc->sasl_password);
		} else if (cstr_eq(uc->network.sasl.mechanism, "PLAIN")) {
			strbuf_append_char(&response, '\0');
			strbuf_append_cstr(&response, uc->network.sasl.plain_username);
			strbuf_append_char(&response, '\0');
			strbuf_append_cstr(&response, uc->network.sasl.plain_password);
		} else if (!cstr_eq(uc->network.sasl.mechanism, "EXTERNAL")) {
			upstream_send_take(uc, IRC_MSG("AUTHENTICATE", "*"));
			return true;
		}
		irc_msg_vec chunks = {0};
		generate_sasl(&chunks, response.buf, response.len);
		for (size_t i = 0; i < chunks.len; i++) {
			upstream_send_take(uc, chunks.data[i]);
		}
		if (response.buf != nullptr) {
			explicit_bzero(response.buf, response.len);
		}
		vec_free(&chunks);
		uc->sasl_started = true;
	} else if (cstr_eq(cmd, "900") || cstr_eq(cmd, "901")) {
		if (msg->params.len < (cstr_eq(cmd, "900") ? 3U : 2U)) {
			return true;
		}
		replace(&uc->account, cstr_eq(cmd, "900") ? irc_msg_param(msg, 2) : "");
		struct str name, user, host;
		irc_prefix_split(str_from(irc_msg_param(msg, 1)), &name, &user, &host);
		xfree(uc->username);
		xfree(uc->hostname);
		uc->username = str_dup(user);
		uc->hostname = str_dup(host);
		upstream_notify_state(uc);
	} else if (cstr_eq(cmd, "396")) {
		if (msg->params.len >= 2) {
			struct str user, host;
			if (str_split_once(str_from(irc_msg_param(msg, 1)), '@', &user, &host)) {
				xfree(uc->username);
				uc->username = str_dup(user);
			} else {
				host = str_from(irc_msg_param(msg, 1));
			}
			xfree(uc->hostname);
			uc->hostname = str_dup(host);
			upstream_notify_state(uc);
		}
	} else if (cstr_eq(cmd, "902") || cstr_eq(cmd, "903") || cstr_eq(cmd, "904") ||
		   cstr_eq(cmd, "905") || cstr_eq(cmd, "906") || cstr_eq(cmd, "907")) {
		uc->sasl_active = uc->sasl_started = false;
		if (!uc->registered) {
			upstream_send_take(uc, IRC_MSG("CAP", "END"));
		} else {
			uint64_t id = 0;
			upstream_route_reply(uc, msg, &id);
			upstream_emit_take(uc, irc_msg_clone(msg), id);
		}
	} else if (cstr_eq(cmd, "001")) {
		if (msg->params.len == 0) {
			return true;
		}
		replace(&uc->nick, irc_msg_param(msg, 0));
		replace(&uc->server_name, irc_msg_prefix_name(msg));
		uc->registered = true;
		uc->state = UPSTREAM_ONLINE;
		uc->deadline = 0;
		backoffer_reset(&uc->reconnect_backoff);
		uc->reconnect_backoff.n = 1;
		if (uc->network.connect_commands.len > 0) {
			uc->command_at = uc->now;
			if (uc->command_at == 0) {
				uc->command_at = 1;
			}
		} else if (cstr_eq(uc->network.sasl.mechanism, "EXTERNAL") &&
			   !upstream_supports_sasl(uc, "EXTERNAL")) {
			uc->attach_at = uc->now + 3000;
		} else {
			uc->attached = true;
		}
		upstream_notify_state(uc);
	} else if (cstr_eq(cmd, "005")) {
		handle_isupport(uc, msg);
	} else if (cstr_eq(cmd, "376") || cstr_eq(cmd, "422")) {
		if (uc->got_motd) {
			return false;
		}
		uc->got_motd = true;
		uc->regain_at = uc->now + 30000;
	} else if (cstr_eq(cmd, "002") || cstr_eq(cmd, "003") || cstr_eq(cmd, "004") ||
		   (!uc->got_motd &&
		    (cstr_eq(cmd, "375") || cstr_eq(cmd, "372") || cstr_eq(cmd, "251") ||
		     cstr_eq(cmd, "252") || cstr_eq(cmd, "253") || cstr_eq(cmd, "254") ||
		     cstr_eq(cmd, "255") || cstr_eq(cmd, "265") || cstr_eq(cmd, "266") ||
		     cstr_eq(cmd, "250") || cstr_eq(cmd, "243") || cstr_eq(cmd, "246") ||
		     cstr_eq(cmd, "247")))) {
		// Our welcome replaces upstream's initial registration burst.
	} else {
		return false;
	}
	return true;
}
