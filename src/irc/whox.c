#include "irc/whox.h"

#include "base/alloc.h"
#include "base/utf8.h"
#include "irc/numerics.h"

// All WHOX field letters, by order of appearance in RPL_WHOSPCRPL messages
static const char WHOX_FIELDS[] = "tcuihsnfdlaor";

void whox_info_init(struct whox_info *info) {
	*info = (struct whox_info){
		.token = xstrdup(""),
		.channel = xstrdup(""),
		.username = xstrdup(""),
		.hostname = xstrdup(""),
		.server = xstrdup(""),
		.nickname = xstrdup(""),
		.flags = xstrdup(""),
		.account = xstrdup(""),
		.realname = xstrdup(""),
	};
}

void whox_info_free(struct whox_info *info) {
	xfree(info->token);
	xfree(info->channel);
	xfree(info->username);
	xfree(info->hostname);
	xfree(info->server);
	xfree(info->nickname);
	xfree(info->flags);
	xfree(info->account);
	xfree(info->realname);
	*info = (struct whox_info){0};
}

static char **field_ptr(struct whox_info *info, char k) {
	switch (k) {
	case 't':
		return &info->token;
	case 'c':
		return &info->channel;
	case 'u':
		return &info->username;
	case 'h':
		return &info->hostname;
	case 's':
		return &info->server;
	case 'n':
		return &info->nickname;
	case 'f':
		return &info->flags;
	case 'a':
		return &info->account;
	case 'r':
		return &info->realname;
	default:
		return nullptr;
	}
}

void whox_info_set(struct whox_info *info, char k, const char *v) {
	char **p = field_ptr(info, k);
	if (p != nullptr) {
		char *nv = xstrdup(v);
		xfree(*p);
		*p = nv;
	}
}

// The hostname cannot start with a colon as this would get parsed as a
// trailing parameter. IPv6 addresses such as "::1" are prefixed with a zero
// to ensure this.
static char *escape_hostname(const char *hostname) {
	return hostname[0] == ':' ? xasprintf("0%s", hostname) : xstrdup(hostname);
}

static char *get_field(const struct whox_info *info, char k) {
	switch (k) {
	case 't':
		return xstrdup(info->token);
	case 'c':
		return xstrdup(info->channel[0] == '\0' ? "*" : info->channel);
	case 'u':
		return xstrdup(info->username);
	case 'i':
		return xstrdup("255.255.255.255");
	case 'h':
		return escape_hostname(info->hostname);
	case 's':
		return xstrdup(info->server);
	case 'n':
		return xstrdup(info->nickname);
	case 'f':
		return xstrdup(info->flags);
	case 'd':
	case 'l':
	case 'o':
		return xstrdup("0");
	case 'a':
		// WHOX uses "0" to mean "no account"
		if (info->account[0] != '\0' && !cstr_eq(info->account, "*")) {
			return xstrdup(info->account);
		}
		return xstrdup("0");
	case 'r':
		return xstrdup(info->realname);
	default:
		return xstrdup("");
	}
}

struct irc_msg *generate_whox_reply(const char *fields, const struct whox_info *info) {
	if (fields[0] == '\0') {
		const char *channel = info->channel[0] == '\0' ? "*" : info->channel;
		AUTOFREE char *host = escape_hostname(info->hostname);
		AUTOFREE char *real = xasprintf("0 %s", info->realname);
		return IRC_MSG(RPL_WHOREPLY, "*", channel, info->username, host, info->server,
			       info->nickname, info->flags, real);
	}
	struct irc_msg *m = IRC_MSG(RPL_WHOSPCRPL, "*");
	for (const char *f = WHOX_FIELDS; *f != '\0'; f++) {
		if (strchr(fields, *f) != nullptr) {
			AUTOFREE char *v = get_field(info, *f);
			irc_msg_add_param(m, v);
		}
	}
	return m;
}

void parse_whox_options(struct str options, char **fields, char **token) {
	*fields = xstrdup("");
	*token = xstrdup("");
	// TODO: add support for WHOX flags before the '%'
	struct str before, rest;
	if (!str_split_once(options, '%', &before, &rest)) {
		return;
	}
	struct str f = rest, t;
	bool has_token = str_split_once(rest, ',', &f, &t);
	struct strbuf lower = STRBUF_INIT;
	utf8_to_lower(&lower, f);
	xfree(*fields);
	*fields = strbuf_take(&lower);
	if (has_token && strchr(*fields, 't') != nullptr) {
		xfree(*token);
		*token = str_dup(t);
	}
}

bool parse_whox_reply(const struct irc_msg *msg, const char *fields, struct whox_info *info,
		      struct err **err) {
	if (!cstr_eq(msg->command, RPL_WHOSPCRPL)) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(msg->command));
		return err_set(err, 0, "invalid WHOX reply %s", q);
	}
	if (msg->params.len == 0) {
		return err_set(err, 0, "invalid RPL_WHOSPCRPL: no params");
	}
	struct whox_info tmp;
	whox_info_init(&tmp);
	size_t next = 1;
	for (const char *f = WHOX_FIELDS; *f != '\0'; f++) {
		if (strchr(fields, *f) == nullptr) {
			continue;
		}
		if (next >= msg->params.len) {
			whox_info_free(&tmp);
			return err_set(err, 0,
				       "invalid RPL_WHOSPCRPL: missing value for field '%c'", *f);
		}
		whox_info_set(&tmp, *f, msg->params.data[next++]);
	}
	*info = tmp;
	return true;
}
