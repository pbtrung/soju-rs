#include "net/url.h"

#include "base/alloc.h"

bool url_parse(struct str s, struct url *out, struct err **err) {
	struct str fragment = STR_EMPTY, query = STR_EMPTY, a;
	if (str_split_once(s, '#', &a, &fragment)) {
		s = a;
	}
	if (str_split_once(s, '?', &a, &query)) {
		s = a;
	}
	struct str scheme, rest;
	if (!str_split_once_str(s, STR_LIT("://"), &scheme, &rest)) {
		return err_set(err, 0, "missing scheme");
	}
	bool valid = scheme.len > 0 && ((scheme.p[0] >= 'a' && scheme.p[0] <= 'z') ||
					(scheme.p[0] >= 'A' && scheme.p[0] <= 'Z'));
	for (size_t i = 0; valid && i < scheme.len; i++) {
		char c = scheme.p[i];
		valid = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
			(c >= '0' && c <= '9') || c == '+' || c == '-' || c == '.';
	}
	if (!valid) {
		return err_set(err, 0, "first path segment in URL cannot contain colon");
	}
	struct str authority = rest, path = STR_EMPTY;
	ptrdiff_t slash = str_index(rest, '/');
	if (slash >= 0) {
		authority = str_slice(rest, 0, (size_t)slash);
		path = str_slice(rest, (size_t)slash, rest.len);
	}
	struct str host = authority;
	ptrdiff_t at = str_rindex(authority, '@');
	if (at >= 0) {
		host = str_slice(authority, (size_t)at + 1, authority.len);
	}
	if (str_contains(host, ' ')) {
		return err_set(err, 0, "invalid character \" \" in host name");
	}
	char *sch = str_dup(scheme);
	for (char *c = sch; *c != '\0'; c++) {
		if (*c >= 'A' && *c <= 'Z') {
			*c = (char)(*c - 'A' + 'a');
		}
	}
	*out = (struct url){
		.scheme = sch,
		.host = str_dup(host),
		.path = str_dup(path),
		.has_user = at >= 0,
		.query = str_dup(query),
		.fragment = str_dup(fragment),
	};
	return true;
}

void url_free(struct url *u) {
	xfree(u->scheme);
	xfree(u->host);
	xfree(u->path);
	xfree(u->query);
	xfree(u->fragment);
	*u = (struct url){0};
}

bool network_url(const char *addr, struct url *out, struct err **err) {
	AUTOFREE char *s =
		strstr(addr, "://") != nullptr ? xstrdup(addr) : xasprintf("ircs://%s", addr);
	struct err *e = nullptr;
	if (!url_parse(str_from(s), out, &e)) {
		err_set(err, 0, "failed to parse upstream server URL: %s", err_msg(e));
		err_free(e);
		return false;
	}
	if (cstr_eq(out->scheme, "irc+unix") || cstr_eq(out->scheme, "unix")) {
		char *p = xasprintf("%s%s", out->host, out->path);
		xfree(out->path);
		out->path = p;
		xfree(out->host);
		out->host = xstrdup("");
	}
	return true;
}

bool split_host_port(struct str hostport, char **host, char **port, struct err **err) {
	if (hostport.len > 0 && hostport.p[0] == '[') {
		struct str rest = str_slice(hostport, 1, hostport.len);
		ptrdiff_t end = str_index(rest, ']');
		if (end < 0) {
			return err_set(err, 0, "address %.*s: missing ']' in address",
				       STR_FMT(hostport));
		}
		struct str after = str_slice(rest, (size_t)end + 1, rest.len);
		if (after.len == 0 || after.p[0] != ':') {
			return err_set(err, 0, "address %.*s: missing port in address",
				       STR_FMT(hostport));
		}
		*host = str_dup(str_slice(rest, 0, (size_t)end));
		*port = str_dup(str_slice(after, 1, after.len));
		return true;
	}
	ptrdiff_t i = str_rindex(hostport, ':');
	if (i < 0) {
		return err_set(err, 0, "address %.*s: missing port in address", STR_FMT(hostport));
	}
	struct str h = str_slice(hostport, 0, (size_t)i);
	if (str_contains(h, ':')) {
		return err_set(err, 0, "address %.*s: too many colons in address",
			       STR_FMT(hostport));
	}
	*host = str_dup(h);
	*port = str_dup(str_slice(hostport, (size_t)i + 1, hostport.len));
	return true;
}

char *join_host_port(const char *host, const char *port) {
	if (strchr(host, ':') != nullptr) {
		return xasprintf("[%s]:%s", host, port);
	}
	return xasprintf("%s:%s", host, port);
}
