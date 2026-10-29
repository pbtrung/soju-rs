#include "http/forwarded.h"

#include "base/alloc.h"
#include "base/base64.h"
#include "base/path_match.h"
#include "base/utf8.h"
#include "net/ipaddr.h"
#include "net/url.h"

// HeaderValue::to_str: only visible ASCII and tabs
static bool header_str_ok(struct str v) {
	for (size_t i = 0; i < v.len; i++) {
		unsigned char c = (unsigned char)v.p[i];
		if (c != '\t' && (c < 32 || c >= 127)) {
			return false;
		}
	}
	return true;
}

static bool is_token_char(char c) {
	return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
	       (c != '\0' && strchr("!#$%&'*+-.^_`|~", c) != nullptr);
}

char *parse_x_forwarded_for(struct header_values xff) {
	struct str last = STR_EMPTY;
	bool found = false;
	for (size_t i = 0; i < xff.n; i++) {
		if (!header_str_ok(xff.v[i])) {
			continue;
		}
		struct str rest = xff.v[i], item;
		while (str_next_field(&rest, ',', &item)) {
			item = utf8_trim(item);
			if (item.len > 0) {
				last = item;
				found = true;
			}
		}
	}
	if (!found) {
		return nullptr;
	}
	struct ipaddr a;
	uint16_t port;
	if (ipaddr_parse(last, &a) || socketaddr_parse(last, &a, &port)) {
		return str_dup(last);
	}
	return nullptr;
}

char *parse_forwarded(struct header_values forwarded, struct header_values xff) {
	if (forwarded.n == 0 || !header_str_ok(forwarded.v[0])) {
		return parse_x_forwarded_for(xff);
	}
	// Like Go's mime.ParseMediaType("hack; " + forwarded): a single element
	// of semicolon-separated key=value pairs
	char *result = nullptr;
	struct str rest = forwarded.v[0], part;
	while (str_next_field(&rest, ';', &part)) {
		part = utf8_trim(part);
		if (part.len == 0) {
			continue;
		}
		struct str k, v;
		if (!str_split_once(part, '=', &k, &v)) {
			goto fail;
		}
		AUTO_STRBUF struct strbuf key = STRBUF_INIT;
		utf8_to_lower(&key, utf8_trim(k));
		v = utf8_trim(v);
		for (size_t i = 0; i < key.len; i++) {
			if (!is_token_char(key.buf[i])) {
				goto fail;
			}
		}
		char *value;
		if (v.len > 0 && v.p[0] == '"') {
			struct str q = str_slice(v, 1, v.len);
			if (q.len == 0 || q.p[q.len - 1] != '"') {
				goto fail;
			}
			q.len--;
			struct strbuf sb = STRBUF_INIT;
			for (size_t i = 0; i < q.len; i++) {
				if (q.p[i] == '\\' && i + 1 < q.len && q.p[i + 1] == '"') {
					strbuf_append_char(&sb, '"');
					i++;
				} else {
					strbuf_append_char(&sb, q.p[i]);
				}
			}
			strbuf_append(&sb, "", 0);
			value = strbuf_take(&sb);
		} else {
			if (str_contains(v, ',') || str_contains(v, ' ') || str_contains(v, '"')) {
				goto fail;
			}
			value = str_dup(v);
		}
		if (cstr_eq(strbuf_cstr(&key), "for")) {
			xfree(result);
			result = value;
		} else {
			xfree(value);
		}
	}
	// The address ends up in IRC prefixes, where spaces would split them
	if (result != nullptr) {
		struct str r = str_from(result);
		bool bad = r.len == 0;
		size_t i = 0;
		uint32_t cp;
		while (!bad && utf8_next(r, &i, &cp)) {
			bad = utf8_is_whitespace(cp) || utf8_is_control(cp);
		}
		if (bad) {
			goto fail;
		}
	}
	return result;
fail:
	xfree(result);
	return nullptr;
}

bool parse_client_cert(struct str s, struct strbuf *out) {
	if (s.len < 2 || s.p[0] != ':' || s.p[s.len - 1] != ':') {
		return false;
	}
	return base64_decode(out, BASE64_STANDARD, str_slice(s, 1, s.len - 1), nullptr);
}

bool authenticate_origin(const char *origin, const char *host, const strvec *patterns,
			 struct err **err) {
	if (origin == nullptr || origin[0] == '\0') {
		return true;
	}
	struct url u;
	struct err *e = nullptr;
	if (!url_parse(str_from(origin), &u, &e)) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(origin));
		err_set(err, 0, "failed to parse Origin header %s: %s", q, err_msg(e));
		err_free(e);
		return false;
	}
	bool ok = false;
	if (host == nullptr) {
		host = "";
	}
	if (str_eq_ignore_ascii_case(str_from(host), str_from(u.host))) {
		ok = true;
		goto out;
	}
	for (size_t i = 0; i < patterns->len; i++) {
		const char *pattern = patterns->data[i];
		AUTOFREE char *target = strstr(pattern, "://") != nullptr
						? xasprintf("%s://%s", u.scheme, u.host)
						: xstrdup(u.host);
		AUTO_STRBUF struct strbuf lp = STRBUF_INIT;
		AUTO_STRBUF struct strbuf lt = STRBUF_INIT;
		utf8_to_lower(&lp, str_from(pattern));
		utf8_to_lower(&lt, str_from(target));
		bool m;
		if (!path_match(strbuf_str(&lp), strbuf_str(&lt), &m)) {
			AUTOFREE char *q = utf8_debug_cstr(str_from(pattern));
			err_set(err, 0, "failed to parse path pattern %s: syntax error in pattern",
				q);
			goto out;
		}
		if (m) {
			ok = true;
			goto out;
		}
	}
	if (u.host[0] == '\0') {
		AUTOFREE char *q = utf8_debug_cstr(str_from(origin));
		err_set(err, 0, "request Origin %s is not a valid URL with a host", q);
		goto out;
	}
	{
		AUTOFREE char *qh = utf8_debug_cstr(str_from(u.host));
		AUTOFREE char *qhost = utf8_debug_cstr(str_from(host));
		err_set(err, 0, "request Origin %s is not authorized for Host %s", qh, qhost);
	}
out:
	url_free(&u);
	return ok;
}
