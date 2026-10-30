// Username unmarshalling, NickServ credentials and BOUNCER network
// attributes (soju-rs downstream/mod.rs)
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

void unmarshal_username(const char *raw, char **username, char **client, char **network) {
	struct str s = str_from(raw);
	ptrdiff_t i = -1, j = -1;
	for (size_t k = 0; k < s.len; k++) {
		if (s.p[k] == '/' || s.p[k] == '@') {
			if (i < 0) {
				i = (ptrdiff_t)k;
			}
			j = (ptrdiff_t)k;
		}
	}
	*username = str_dup(i >= 0 ? str_slice(s, 0, (size_t)i) : s);
	*client = xstrdup("");
	*network = xstrdup("");
	if (j >= 0) {
		struct str after = str_slice(s, (size_t)j + 1, s.len);
		char *v = str_dup(after);
		char **dst = s.p[j] == '@' ? client : network;
		xfree(*dst);
		*dst = v;
	}
	if (i >= 0 && j >= 0 && i < j) {
		char *v = str_dup(str_slice(s, (size_t)i + 1, (size_t)j));
		char **dst = s.p[i] == '@' ? client : network;
		xfree(*dst);
		*dst = v;
	}
}

bool parse_nickserv_credentials(const char *text, const char *nick, char **account,
				char **password) {
	strvec fields = {0};
	struct str rest = str_from(text), w;
	while (utf8_next_word(&rest, &w)) {
		strvec_push_take(&fields, str_dup(w));
	}
	bool ok = false;
	if (fields.len >= 2) {
		AUTO_STRBUF struct strbuf cmd = STRBUF_INIT;
		utf8_to_upper(&cmd, str_from(fields.data[0]));
		const char *c = strbuf_cstr(&cmd);
		size_t np = fields.len - 1;
		char **p = fields.data + 1;
		if (cstr_eq(c, "REGISTER")) {
			*account = xstrdup(nick);
			*password = xstrdup(p[0]);
			ok = true;
		} else if (cstr_eq(c, "IDENTIFY")) {
			*account = xstrdup(np == 1 ? nick : p[0]);
			*password = xstrdup(np == 1 ? p[0] : p[1]);
			ok = true;
		} else if (cstr_eq(c, "SET") && np == 2 &&
			   str_eq_ignore_ascii_case(str_from(p[0]), STR_LIT("PASSWORD"))) {
			// Upstream also reports success for other SET subcommands, with
			// empty credentials which then overwrite the saved ones
			*account = xstrdup(nick);
			*password = xstrdup(p[1]);
			ok = true;
		}
	}
	strvec_free(&fields);
	return ok;
}

void fill_network_addr_attrs(struct irc_tags *attrs, const struct db_network *n) {
	struct url u;
	if (!network_url(n->addr, &u, nullptr)) {
		return;
	}
	bool has_host_port = false;
	if (cstr_eq(u.scheme, "ircs")) {
		irc_tags_set(attrs, "tls", "1");
		has_host_port = true;
	} else if (cstr_eq(u.scheme, "irc+insecure")) {
		irc_tags_set(attrs, "tls", "0");
		has_host_port = true;
	}
	if (has_host_port) {
		char *host, *port;
		if (split_host_port(str_from(u.host), &host, &port, nullptr)) {
			irc_tags_set(attrs, "host", host);
			irc_tags_set(attrs, "port", port);
			xfree(host);
			xfree(port);
		} else {
			irc_tags_set(attrs, "host", u.host);
		}
	}
	url_free(&u);
}

static char *network_addr_from_attrs(const struct irc_tags *attrs) {
	const char *host = irc_tags_get_or_empty(attrs, "host");
	if (host[0] == '\0') {
		return xstrdup("");
	}
	struct strbuf addr = STRBUF_INIT;
	strbuf_append_cstr(&addr, host);
	const char *port = irc_tags_get_or_empty(attrs, "port");
	if (port[0] != '\0') {
		strbuf_printf(&addr, ":%s", port);
	}
	if (cstr_eq(irc_tags_get_or_empty(attrs, "tls"), "0")) {
		AUTOFREE char *a = strbuf_take(&addr);
		return xasprintf("irc+insecure://%s", a);
	}
	return strbuf_take(&addr);
}

bool update_network_attrs(struct db_network *n, const struct irc_tags *attrs,
			  const char *subcommand, struct irc_msg **irc) {
	struct irc_tags addr_attrs = IRC_TAGS_INIT;
	fill_network_addr_attrs(&addr_attrs, n);
	bool update_addr = false;
	bool ok = true;
	for (size_t i = 0; ok && i < attrs->v.len; i++) {
		const char *k = attrs->v.data[i].key, *v = attrs->v.data[i].value;
		if (cstr_eq(k, "host") || cstr_eq(k, "port") || cstr_eq(k, "tls")) {
			update_addr = true;
			irc_tags_set(&addr_attrs, k, v);
		} else if (cstr_eq(k, "name")) {
			replace_str(&n->name, v);
		} else if (cstr_eq(k, "nickname")) {
			replace_str(&n->nick, v);
		} else if (cstr_eq(k, "username")) {
			replace_str(&n->username, v);
		} else if (cstr_eq(k, "realname")) {
			replace_str(&n->realname, v);
		} else if (cstr_eq(k, "pass")) {
			replace_str(&n->pass, v);
		} else {
			*irc = IRC_MSG("FAIL", "BOUNCER", "UNKNOWN_ATTRIBUTE", subcommand, k,
				       "Unknown attribute");
			ok = false;
		}
	}
	if (ok && update_addr) {
		char *addr = network_addr_from_attrs(&addr_attrs);
		xfree(n->addr);
		n->addr = addr;
		if (addr[0] == '\0') {
			*irc = IRC_MSG("FAIL", "BOUNCER", "NEED_ATTRIBUTE", subcommand, "host",
				       "Missing required host attribute");
			ok = false;
		}
	}
	irc_tags_free(&addr_attrs);
	return ok;
}
