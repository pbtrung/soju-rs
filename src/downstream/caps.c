// CAP negotiation and ISUPPORT (soju-rs downstream/caps.rs)
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

// Capabilities which require support from the upstream server
static const char *const PASSTHROUGH_CAPS[][2] = {
	{"account-notify", ""},
	{"account-tag", ""},
	{"away-notify", ""},
	{"chghost", ""},
	{"extended-join", ""},
	{"extended-monitor", ""},
	{"labeled-response", ""},
	{"message-tags", ""},
	{"multi-prefix", ""},
	{"draft/extended-monitor", ""},
	{"draft/message-redaction", ""},
};

static const char *const PERMANENT_ISUPPORT[] = {"soju.im/SAFERATE", "draft/ACCOUNTREQUIRED"};

static const char *const PASSTHROUGH_ISUPPORT[] = {
	"ACCOUNTEXTBAN", "AWAYLEN",    "BOT",           "CASEMAPPING", "CHANLIMIT", "CHANMODES",
	"CHANNELLEN",    "CHANTYPES",  "CLIENTTAGDENY", "ELIST",       "EXCEPTS",   "EXTBAN",
	"HOSTLEN",       "INVEX",      "KICKLEN",       "LINELEN",     "MAXLIST",   "MAXTARGETS",
	"MODES",         "MONITOR",    "NAMELEN",       "NETWORK",     "NICKLEN",   "PREFIX",
	"SAFELIST",      "STATUSMSG",  "TARGMAX",       "TOPICLEN",    "USERLEN",   "UTF8ONLY",
	"WHOX",          "draft/ICON",
};

// CAP LS/LIST: with CAP 302, split over several lines if needed, all but
// the last marked with "*"
static void send_cap_list(struct downstream *dc, const char *subcmd, const strvec *caps) {
	AUTOFREE char *host = server_hostname(dc->srv);
	// Room for ":<prefix> CAP <nick> <subcmd> * :" in 512 bytes; a long
	// bouncer-only nick must not underflow
	size_t overhead = 2 + strlen(host) + 2 + strlen(dc->nick) + strlen(subcmd) + 10;
	size_t max = MAX_MESSAGE_LENGTH > overhead ? MAX_MESSAGE_LENGTH - overhead : 0;
	if (max < 64) {
		max = 64;
	}
	strvec lines = {0};
	strvec_push(&lines, "");
	struct strbuf line = STRBUF_INIT;
	for (size_t i = 0; i < caps->len; i++) {
		const char *cap = caps->data[i];
		if (dc->cap_version >= 302 && line.len > 0 && line.len + 1 + strlen(cap) > max) {
			replace_str(&vec_last(&lines), strbuf_cstr(&line));
			strvec_push(&lines, "");
			strbuf_reset(&line);
			strbuf_append_cstr(&line, cap);
		} else {
			if (line.len > 0) {
				strbuf_append_char(&line, ' ');
			}
			strbuf_append_cstr(&line, cap);
		}
	}
	replace_str(&vec_last(&lines), strbuf_cstr(&line));
	strbuf_free(&line);
	for (size_t i = 0; i < lines.len; i++) {
		struct irc_msg *m = IRC_MSG("CAP", dc->nick, subcmd);
		if (i + 1 < lines.len) {
			irc_msg_add_param(m, "*");
		}
		irc_msg_add_param(m, lines.data[i]);
		irc_msg_set_prefix(m, host, "", "");
		downstream_send(dc, m);
	}
	strvec_free(&lines);
}

static void caps_text(struct downstream *dc, bool enabled, strvec *out) {
	const struct strmap *m = enabled ? &dc->caps.enabled : &dc->caps.available;
	const char *k;
	void *v;
	for (size_t it = 0; strmap_next(m, &it, &k, &v);) {
		if (!enabled && dc->cap_version >= 302 && v != nullptr && ((char *)v)[0] != '\0') {
			strvec_push_take(out, xasprintf("%s=%s", k, (char *)v));
		} else {
			strvec_push(out, k);
		}
	}
}

struct result downstream_handle_cap(struct downstream *dc, const struct irc_msg *msg) {
	if (!need_params(msg, 1)) {
		return need_more_params(msg->command);
	}
	AUTO_STRBUF struct strbuf cmd_b = STRBUF_INIT;
	utf8_to_upper(&cmd_b, str_from(msg->params.data[0]));
	const char *cmd = strbuf_cstr(&cmd_b);
	if (cstr_eq(cmd, "LS")) {
		if (msg->params.len > 1) {
			long long v;
			const char *arg = msg->params.data[1];
			if (!str_parse_i64(str_from(arg), &v)) {
				AUTOFREE char *q = utf8_debug_cstr(str_from(arg));
				return (struct result){
					.fatal = xasprintf(
						"strconv.Atoi: parsing %s: invalid syntax", q)};
			}
			dc->cap_version = v;
		}
		if (!dc->registered && dc->cap_version >= 302) {
			// Let downstream show everything it supports, and trim down the
			// available capabilities when upstreams are known
			for (size_t i = 0; i < N(PASSTHROUGH_CAPS); i++) {
				cap_registry_set_available(&dc->caps, PASSTHROUGH_CAPS[i][0],
							   PASSTHROUGH_CAPS[i][1]);
			}
			AUTOFREE char *ml = multiline_limits_cap_value(multiline_limits_default());
			cap_registry_set_available(&dc->caps, MULTILINE_BATCH_TYPE, ml);
		}
		strvec caps = {0};
		caps_text(dc, false, &caps);
		send_cap_list(dc, "LS", &caps);
		strvec_free(&caps);
		if (dc->cap_version >= 302) {
			// CAP version 302 implicitly enables cap-notify
			cap_registry_set_enabled(&dc->caps, "cap-notify", true);
		}
		if (dc->reg != nullptr && !dc->registered) {
			dc->reg->negotiating_caps = true;
		}
	} else if (cstr_eq(cmd, "LIST")) {
		strvec caps = {0};
		caps_text(dc, true, &caps);
		send_cap_list(dc, "LIST", &caps);
		strvec_free(&caps);
	} else if (cstr_eq(cmd, "REQ")) {
		if (msg->params.len < 2) {
			return irc_err(IRC_MSG(ERR_INVALIDCAPCMD, dc->nick, cmd,
					       "Missing argument in CAP REQ command"));
		}
		const char *arg = msg->params.data[1];
		bool ack = true;
		strvec names = {0};
		VEC(bool) enables = {0};
		struct str rest = str_from(arg), word;
		while (ack && utf8_next_word(&rest, &word)) {
			struct strbuf lower = STRBUF_INIT;
			utf8_to_lower(&lower, word);
			struct str name = strbuf_str(&lower);
			bool enable = true;
			if (name.len > 0 && name.p[0] == '-') {
				name = str_slice(name, 1, name.len);
				enable = false;
			}
			AUTOFREE char *n = str_dup(name);
			strbuf_free(&lower);
			if (enable == cap_registry_is_enabled(&dc->caps, n)) {
				continue;
			}
			if (!cap_registry_is_available(&dc->caps, n) ||
			    // cap-notify cannot be disabled with CAP version 302
			    (cstr_eq(n, "cap-notify") && dc->cap_version >= 302 && !enable) ||
			    // account-required is an informational cap
			    cstr_eq(n, "soju.im/account-required")) {
				ack = false;
				break;
			}
			strvec_push(&names, n);
			vec_push(&enables, enable);
		}
		// Atomically ack the whole capability set
		if (ack) {
			for (size_t i = 0; i < names.len; i++) {
				cap_registry_set_enabled(&dc->caps, names.data[i], enables.data[i]);
			}
		}
		strvec_free(&names);
		vec_free(&enables);
		struct irc_msg *m = IRC_MSG("CAP", dc->nick, ack ? "ACK" : "NAK", arg);
		server_set_prefix(dc->srv, m);
		downstream_send(dc, m);
		if (dc->reg != nullptr && !dc->registered) {
			dc->reg->negotiating_caps = true;
		}
	} else if (cstr_eq(cmd, "END")) {
		if (dc->reg != nullptr && !dc->registered) {
			dc->reg->negotiating_caps = false;
		}
	} else {
		return irc_err(IRC_MSG(ERR_INVALIDCAPCMD, dc->nick, cmd, "Unknown CAP command"));
	}
	return ok();
}

void downstream_build_isupport(struct downstream *dc, const struct isupport_ctx *ctx,
			       struct isupport *out) {
	struct server_config *cfg = server_config_get(dc->srv);
	const struct config_basic *b = &cfg->cfg.basic;
	for (size_t i = 0; i < N(PERMANENT_ISUPPORT); i++) {
		isupport_set(out, PERMANENT_ISUPPORT[i], nullptr);
	}
	if (ctx->has_network) {
		char id[24];
		snprintf(id, sizeof(id), "%lld", (long long)ctx->network_id);
		isupport_set(out, "BOUNCER_NETID", id);
	}
	if (!ctx->has_network && b->title[0] != '\0') {
		isupport_set(out, "NETWORK", b->title);
	}
	if (!ctx->has_network && cfg->icon_url[0] != '\0') {
		isupport_set(out, "draft/ICON", cfg->icon_url);
	} else if (!ctx->has_network && cfg->icon_path[0] != '\0') {
		AUTOFREE char *icon = xasprintf("%s/icon", b->http_ingress);
		isupport_set(out, "draft/ICON", icon);
	}
	if (ctx->has_user) {
		if (!ctx->has_network) {
			isupport_set(out, "BOT", "B");
			isupport_set(out, "CASEMAPPING", "ascii");
			isupport_set(out, "WHOX", nullptr);
			// channels are not supported
			isupport_set(out, "CHANTYPES", "");
			// default bufio.Reader size
			isupport_set(out, "LINELEN", "4096");
		}
		if (ctx->chathistory && ctx->has_network) {
			char limit[24];
			snprintf(limit, sizeof(limit), "%zu", b->chat_history_limit);
			isupport_set(out, "CHATHISTORY", limit);
			isupport_set(out, "MSGREFTYPES", "timestamp");
		}
		if (cap_registry_is_enabled(&dc->caps, "soju.im/webpush") &&
		    dc->srv->web_push != nullptr) {
			isupport_set(out, "VAPID", dc->srv->web_push->vapid_public);
		}
	}
	if (ctx->up_isupport != nullptr) {
		// If upstream doesn't support message-tags, indicate that we'll drop
		// all of them
		if (!strmap_has(&ctx->up_isupport->m, "CLIENTTAGDENY") &&
		    (ctx->up_caps == nullptr ||
		     !cap_registry_is_enabled(ctx->up_caps, "message-tags"))) {
			isupport_set(out, "CLIENTTAGDENY", "*");
		}
		for (size_t i = 0; i < N(PASSTHROUGH_ISUPPORT); i++) {
			const char *k = PASSTHROUGH_ISUPPORT[i];
			if (strmap_has(&ctx->up_isupport->m, k)) {
				isupport_set(out, k, strmap_get(&ctx->up_isupport->m, k));
			}
		}
	}
	server_config_unref(cfg);
}

void downstream_send_isupport(struct downstream *dc, const struct isupport *is) {
	irc_msg_vec msgs = {0};
	generate_isupport(&msgs, is);
	if (cap_registry_is_enabled(&dc->caps, "draft/extended-isupport")) {
		downstream_send_batch(dc, "draft/isupport", nullptr, 0, &msgs);
	} else {
		for (size_t i = 0; i < msgs.len; i++) {
			downstream_send(dc, msgs.data[i]);
		}
		vec_free(&msgs);
	}
}
