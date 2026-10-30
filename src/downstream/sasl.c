// SASL authentication of downstream connections (soju-rs
// downstream/sasl.rs): the server side of each mechanism
#include <string.h>

#include "base/alloc.h"
#include "base/utf8.h"
#include "downstream/internal.h"

#include "base/base64.h"

void downstream_sasl_free(struct downstream_sasl *s) {
	if (s == nullptr) {
		return;
	}
	xfree(s->mechanism);
	xfree(s->oauth_fail);
	if (s->has_plain) {
		xfree(s->plain.identity);
		xfree(s->plain.username);
		if (s->plain.password != nullptr) {
			explicit_bzero(s->plain.password, strlen(s->plain.password));
		}
		xfree(s->plain.password);
	}
	if (s->has_oauth) {
		xfree(s->oauth.username);
		xfree(s->oauth.token);
	}
	strbuf_free(&s->pending_resp);
	xfree(s);
}

static char *lossy(struct str s) {
	struct strbuf sb = STRBUF_INIT;
	utf8_lossy(&sb, s);
	return strbuf_take(&sb);
}

static struct strbuf *new_challenge(const char *p, size_t n) {
	struct strbuf *c = xcalloc(1, sizeof(*c));
	strbuf_append(c, p, n);
	return c;
}

static bool oauth_fail(struct downstream_sasl *s, const char *descr, struct strbuf **challenge) {
	xfree(s->oauth_fail);
	s->oauth_fail = xasprintf("sasl: client error: %s", descr);
	static const char reply[] = "{\"status\":\"invalid_request\",\"schemes\":\"bearer\","
				    "\"scope\":\"\"}";
	*challenge = new_challenge(reply, sizeof(reply) - 1);
	return true;
}

static bool next_oauth(struct downstream_sasl *s, struct str r, struct strbuf **challenge) {
	struct str flag, authzid, rest, tmp;
	if (!str_split_once(r, ',', &flag, &tmp) || !str_split_once(tmp, ',', &authzid, &rest)) {
		return oauth_fail(s, "Invalid response", challenge);
	}
	if (!str_eq_cstr(flag, "n")) {
		return oauth_fail(s, "Invalid response, missing 'n' in gs2-cb-flag", challenge);
	}
	struct oauth_bearer o = {.username = xstrdup(""), .token = xstrdup("")};
	if (authzid.len > 0) {
		if (!str_starts_with(authzid, STR_LIT("a="))) {
			xfree(o.username);
			xfree(o.token);
			return oauth_fail(s, "Invalid response, missing 'a=' in gs2-authzid",
					  challenge);
		}
		xfree(o.username);
		o.username = lossy(str_slice(authzid, 2, authzid.len));
	}
	struct str parts = rest, p;
	while (str_next_field(&parts, '\x01', &p)) {
		if (p.len == 0) {
			continue;
		}
		AUTOFREE char *ps = lossy(p);
		struct str k, v;
		const char *descr = nullptr;
		AUTOFREE char *unknown = nullptr;
		if (!str_split_once(str_from(ps), '=', &k, &v)) {
			descr = "Invalid response, missing '='";
		} else if (str_eq_cstr(k, "host")) {
		} else if (str_eq_cstr(k, "port")) {
			unsigned long long port;
			// v.parse::<u16>()
			if (!str_parse_u64(v, &port) || port > 65535) {
				descr = "Invalid response, malformed 'port' value";
			}
		} else if (str_eq_cstr(k, "auth")) {
			AUTO_STRBUF struct strbuf lower = STRBUF_INIT;
			utf8_to_lower(&lower, v);
			if (!str_starts_with(strbuf_str(&lower), STR_LIT("bearer "))) {
				descr = "Unsupported token type";
			} else {
				xfree(o.token);
				o.token = str_dup(str_slice(v, 7, v.len));
			}
		} else {
			unknown =
				xasprintf("Invalid response, unknown parameter: %.*s", STR_FMT(k));
			descr = unknown;
		}
		if (descr != nullptr) {
			xfree(o.username);
			xfree(o.token);
			return oauth_fail(s, descr, challenge);
		}
	}
	s->oauth = o;
	s->has_oauth = true;
	return true;
}

bool downstream_sasl_next(struct downstream_sasl *s, const struct str *response,
			  struct strbuf **challenge, struct err **err) {
	*challenge = nullptr;
	if (s->kind == SASL_OAUTHBEARER && s->oauth_fail != nullptr) {
		if (response == nullptr ||
		    (response->len != 1 && (response->len == 0 || response->p[0] != 0x01))) {
			return err_set(err, 0, "sasl: invalid response");
		}
		return err_set(err, 0, "%s", s->oauth_fail);
	}
	if (s->done) {
		return err_set(err, 0, "sasl: unexpected client response");
	}
	if (response == nullptr) {
		*challenge = new_challenge("", 0);
		return true;
	}
	s->done = true;
	struct str r = *response;
	switch (s->kind) {
	case SASL_PLAIN: {
		struct str a, rest, b, c;
		if (!str_split_once(r, '\0', &a, &rest) || !str_split_once(rest, '\0', &b, &c) ||
		    memchr(c.p, '\0', c.len) != nullptr) {
			return err_set(err, 0, "sasl: invalid response");
		}
		s->plain = (struct sasl_plain){
			.identity = lossy(a), .username = lossy(b), .password = lossy(c)};
		s->has_plain = true;
		return true;
	}
	case SASL_ANONYMOUS:
		return true;
	case SASL_EXTERNAL:
		if (memchr(r.p, '\0', r.len) != nullptr) {
			return err_set(err, 0, "sasl: identity contains a NUL character");
		}
		return true;
	case SASL_OAUTHBEARER:
		return next_oauth(s, r, challenge);
	}
	return true;
}

// AUTHENTICATE: returns true with *complete set once the client has sent
// its credentials
struct result downstream_handle_authenticate(struct downstream *dc, const struct irc_msg *msg,
					     bool *complete) {
	*complete = false;
	if (!cap_registry_is_enabled(&dc->caps, "sasl")) {
		return irc_err(
			IRC_MSG(ERR_SASLFAIL, dc->nick,
				"AUTHENTICATE requires the \"sasl\" capability to be enabled"));
	}
	if (msg->params.len == 0) {
		return irc_err(IRC_MSG(ERR_SASLFAIL, dc->nick, "Missing AUTHENTICATE argument"));
	}
	const char *param = msg->params.data[0];
	if (cstr_eq(param, "*")) {
		return irc_err(IRC_MSG(ERR_SASLABORTED, dc->nick, "SASL authentication aborted"));
	}
	struct strbuf decoded = STRBUF_INIT;
	bool has_resp = false;
	if (dc->sasl == nullptr) {
		AUTO_STRBUF struct strbuf mech = STRBUF_INIT;
		utf8_to_upper(&mech, str_from(param));
		enum sasl_kind kind;
		const char *m = strbuf_cstr(&mech);
		if (cstr_eq(m, "PLAIN")) {
			kind = SASL_PLAIN;
		} else if (cstr_eq(m, "OAUTHBEARER")) {
			kind = SASL_OAUTHBEARER;
		} else if (cstr_eq(m, "ANONYMOUS")) {
			kind = SASL_ANONYMOUS;
		} else if (cstr_eq(m, "EXTERNAL")) {
			kind = SASL_EXTERNAL;
		} else {
			return irc_err(
				IRC_MSG(ERR_SASLFAIL, dc->nick, "Unsupported SASL mechanism"));
		}
		dc->sasl = xcalloc(1, sizeof(*dc->sasl));
		dc->sasl->kind = kind;
		dc->sasl->mechanism = xstrdup(m);
	} else {
		struct downstream_sasl *s = dc->sasl;
		const char *chunk = cstr_eq(param, "+") ? "" : param;
		if (s->pending_resp.len + strlen(chunk) > (size_t)10 * 1024) {
			return irc_err(IRC_MSG(ERR_SASLFAIL, dc->nick, "Response too long"));
		}
		strbuf_append_cstr(&s->pending_resp, chunk);
		if (strlen(chunk) == MAX_SASL_LENGTH) {
			return ok(); // Multi-line response, wait for the next command
		}
		if (!base64_decode(&decoded, BASE64_STANDARD, strbuf_str(&s->pending_resp),
				   nullptr)) {
			strbuf_free(&decoded);
			return irc_err(
				IRC_MSG(ERR_SASLFAIL, dc->nick, "Invalid base64-encoded response"));
		}
		strbuf_reset(&s->pending_resp);
		has_resp = true;
	}
	struct str resp = strbuf_str(&decoded);
	struct strbuf *challenge;
	struct err *e = nullptr;
	bool ok_next = downstream_sasl_next(dc->sasl, has_resp ? &resp : nullptr, &challenge, &e);
	strbuf_free(&decoded);
	if (!ok_next) {
		struct result r = {.fatal = xstrdup(err_msg(e))};
		err_free(e);
		return r;
	}
	if (challenge == nullptr) {
		*complete = true;
		return ok();
	}
	AUTOFREE char *s = challenge->len == 0 ? xstrdup("+")
					       : base64_encode_cstr(BASE64_STANDARD, challenge->buf,
								    challenge->len);
	strbuf_free(challenge);
	xfree(challenge);
	// TODO (upstream): multi-line messages
	downstream_send(dc, IRC_MSG("AUTHENTICATE", s));
	return ok();
}
