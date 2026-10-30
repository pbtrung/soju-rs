#include "downstream/downstream.h"

#include "base/alloc.h"
#include "irc/genmsg.h"
#include "irc/numerics.h"

static void replace(char **s, const char *v) {
	char *copy = xstrdup(v);
	xfree(*s);
	*s = copy;
}

static void send_server(struct downstream *dc, struct irc_msg *m) {
	server_set_prefix(dc->srv, m);
	downstream_send(dc, m);
}

static void send_self(struct downstream *dc, struct irc_msg *m) {
	irc_msg_set_prefix(m, dc->nick, dc->username, dc->hostname);
	downstream_send(dc, m);
}

void downstream_update_caps(struct downstream *dc, const struct downstream_sync *s) {
	static const char *const passthrough[] = {"account-notify",
						  "account-tag",
						  "away-notify",
						  "chghost",
						  "extended-join",
						  "extended-monitor",
						  "labeled-response",
						  "message-tags",
						  "multi-prefix",
						  "draft/extended-monitor",
						  "draft/message-redaction"};
	for (size_t i = 0; i < sizeof(passthrough) / sizeof(*passthrough); i++) {
		const char *name = passthrough[i];
		if (s->up_caps == nullptr || cap_registry_is_enabled(s->up_caps, name)) {
			downstream_set_supported_cap(dc, name, "");
		} else {
			downstream_unset_supported_cap(dc, name);
		}
	}
	if (s->has_network) {
		const char *mechs =
			s->up_caps != nullptr ? cap_registry_value(s->up_caps, "sasl") : nullptr;
		bool plain = false;
		if (mechs != nullptr && cap_registry_is_enabled(s->up_caps, "sasl")) {
			struct str rest = str_from(mechs), part;
			plain = rest.len == 0;
			while (str_next_field(&rest, ',', &part)) {
				plain |= str_eq_cstr(part, "PLAIN");
			}
		}
		if (plain) {
			downstream_set_supported_cap(dc, "sasl", "PLAIN,ANONYMOUS");
		} else {
			downstream_unset_supported_cap(dc, "sasl");
		}
	}
	const char *account =
		s->up_caps != nullptr &&
				cap_registry_is_enabled(s->up_caps, "draft/account-registration")
			? cap_registry_value(s->up_caps, "draft/account-registration")
			: nullptr;
	if (account != nullptr) {
		AUTO_STRBUF struct strbuf value = STRBUF_INIT;
		struct str rest = str_from(account), part;
		while (str_next_field(&rest, ',', &part)) {
			if (str_eq_cstr(part, "before-connect")) {
				continue;
			}
			if (value.len > 0) {
				strbuf_append_char(&value, ',');
			}
			strbuf_append_str(&value, part);
		}
		downstream_set_supported_cap(dc, "draft/account-registration", strbuf_cstr(&value));
	} else {
		downstream_unset_supported_cap(dc, "draft/account-registration");
	}
	if (s->up_caps != nullptr && cap_registry_is_enabled(s->up_caps, "batch") &&
	    cap_registry_is_enabled(s->up_caps, MULTILINE_BATCH_TYPE)) {
		const char *v = cap_registry_value(s->up_caps, MULTILINE_BATCH_TYPE);
		struct multiline_limits limits =
			multiline_limits_parse(str_from(v != nullptr ? v : ""));
		AUTOFREE char *value = multiline_limits_cap_value(limits);
		downstream_set_supported_cap(dc, MULTILINE_BATCH_TYPE, value);
	} else {
		downstream_unset_supported_cap(dc, MULTILINE_BATCH_TYPE);
	}
	if (s->chathistory && s->has_network) {
		downstream_set_supported_cap(dc, "draft/event-playback", "");
	} else {
		downstream_unset_supported_cap(dc, "draft/event-playback");
	}
}

static void update_isupport(struct downstream *dc, const struct downstream_sync *s) {
	struct isupport_ctx ctx = {.has_user = true,
				   .has_network = s->has_network,
				   .network_id = s->network_id,
				   .chathistory = s->chathistory,
				   .up_caps = s->up_caps,
				   .up_isupport = s->up_isupport};
	struct isupport is, changed;
	isupport_init(&is);
	isupport_init(&changed);
	downstream_build_isupport(dc, &ctx, &is);
	const char *key;
	void *v;
	for (size_t it = 0; strmap_next(&is.m, &it, &key, &v);) {
		void **old = strmap_get_slot(&dc->isupport.m, key);
		if (old == nullptr || ((*old == nullptr) != (v == nullptr)) ||
		    (v != nullptr && !cstr_eq(*old, v))) {
			isupport_set(&changed, key, v);
		}
	}
	for (size_t it = 0; strmap_next(&dc->isupport.m, &it, &key, nullptr);) {
		if (!strmap_has(&is.m, key)) {
			AUTOFREE char *negated = xasprintf("-%s", key);
			isupport_set(&changed, negated, nullptr);
		}
	}
	if (strmap_len(&changed.m) > 0) {
		downstream_send_isupport(dc, &changed);
	}
	isupport_free(&changed);
	isupport_free(&dc->isupport);
	dc->isupport = is;
}

void downstream_sync(struct downstream *dc, const struct downstream_sync *s) {
	if (!cstr_eq(dc->nick, s->nick)) {
		send_self(dc, IRC_MSG("NICK", s->nick));
		replace(&dc->nick, s->nick);
	}
	if (s->hostname[0] != '\0' &&
	    (!cstr_eq(dc->hostname, s->hostname) || !cstr_eq(dc->username, s->username))) {
		if (cap_registry_is_enabled(&dc->caps, "chghost")) {
			send_self(dc, IRC_MSG("CHGHOST", s->username, s->hostname));
		} else if (!cstr_eq(dc->hostname, s->hostname)) {
			send_server(dc, IRC_MSG(RPL_VISIBLEHOST, dc->nick, s->hostname,
						"is now your visible host"));
		}
		replace(&dc->hostname, s->hostname);
		replace(&dc->username, s->username);
	}
	if (!cstr_eq(dc->realname, s->realname) && cap_registry_is_enabled(&dc->caps, "setname")) {
		send_self(dc, IRC_MSG("SETNAME", s->realname));
		replace(&dc->realname, s->realname);
	}
	if (!cstr_eq(dc->account, s->account) && cap_registry_is_enabled(&dc->caps, "sasl")) {
		AUTO_STRBUF struct strbuf prefix = STRBUF_INIT;
		struct irc_prefix p = {
			.name = dc->nick, .user = dc->username, .host = dc->hostname};
		irc_prefix_format(&p, &prefix);
		if (s->account[0] != '\0') {
			AUTOFREE char *text = xasprintf("You are logged in as %s", s->account);
			send_server(dc, IRC_MSG(RPL_LOGGEDIN, dc->nick, strbuf_cstr(&prefix),
						s->account, text));
		} else {
			send_server(dc, IRC_MSG(RPL_LOGGEDOUT, dc->nick, strbuf_cstr(&prefix),
						"You are logged out"));
		}
		replace(&dc->account, s->account);
	}
	downstream_update_casemapping(dc, s->casemap);
	downstream_update_caps(dc, s);
	update_isupport(dc, s);
}

void downstream_welcome(struct downstream *dc, const struct downstream_sync *s) {
	dc->has_network = s->has_network;
	dc->network_id = s->network_id;
	downstream_registration_done(dc);
	downstream_update_caps(dc, s);
	replace(&dc->nick, s->nick);
	AUTOFREE char *text = xasprintf("Welcome to soju, %s", dc->nick);
	struct server_config *cfg = server_config_get(dc->srv);
	AUTOFREE char *host = xasprintf("Your host is %s", cfg->cfg.basic.hostname);
	send_server(dc, IRC_MSG(RPL_WELCOME, dc->nick, text));
	send_server(dc, IRC_MSG(RPL_YOURHOST, dc->nick, host));
	send_server(dc, IRC_MSG(RPL_MYINFO, dc->nick, cfg->cfg.basic.hostname, "soju", "aiwroO",
				"OovaimnqpsrtklbeI"));
	update_isupport(dc, s);
	if (s->modes != nullptr && s->modes[0] != '\0') {
		AUTOFREE char *modes = xasprintf("+%s", s->modes);
		send_server(dc, IRC_MSG(RPL_UMODEIS, dc->nick, modes));
	} else if (!s->has_network && s->admin) {
		send_server(dc, IRC_MSG(RPL_UMODEIS, dc->nick, "+o"));
	}
	downstream_sync(dc, s);
	if (!s->has_network && cfg->motd[0] != '\0') {
		irc_msg_vec msgs = {0};
		generate_motd(&msgs, str_from(cfg->motd));
		for (size_t i = 0; i < msgs.len; i++) {
			send_server(dc, msgs.data[i]);
		}
		vec_free(&msgs);
	} else {
		send_server(dc, IRC_MSG(ERR_NOMOTD, dc->nick,
					s->has_network ? "Use /motd to read the message of the day"
						       : "No MOTD"));
	}
	server_config_unref(cfg);
}
