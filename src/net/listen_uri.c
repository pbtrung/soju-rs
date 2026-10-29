#include "net/listen_uri.h"

#include "base/alloc.h"
#include "base/utf8.h"
#include "config/config.h"
#include "net/url.h"

static char *with_default_port(const char *addr, const char *port) {
	char *h, *p;
	if (split_host_port(str_from(addr), &h, &p, nullptr)) {
		xfree(h);
		xfree(p);
		return xstrdup(addr);
	}
	return xasprintf("%s:%s", addr, port);
}

// The host without its port, or with brackets trimmed if it has none
static char *hostname_of(const char *host) {
	char *h, *p;
	if (split_host_port(str_from(host), &h, &p, nullptr)) {
		xfree(p);
		return h;
	}
	struct str s = str_from(host);
	while (s.len > 0 && s.p[0] == '[') {
		s = str_slice(s, 1, s.len);
	}
	while (s.len > 0 && s.p[s.len - 1] == ']') {
		s.len--;
	}
	return str_dup(s);
}

static bool is_localhost(const char *host) {
	AUTOFREE char *h = hostname_of(host);
	return cstr_eq(h, "localhost");
}

void listener_spec_free(struct listener_spec *l) {
	xfree(l->label);
	xfree(l->addr);
	*l = (struct listener_spec){0};
}

bool parse_listen_uri(const char *listen, struct listener_spec *out, struct err **err) {
	AUTOFREE char *uri =
		strstr(listen, ":/") != nullptr ? xstrdup(listen) : xasprintf("//%s", listen);
	AUTO_STRBUF struct strbuf scheme_b = STRBUF_INIT;
	struct str scheme_s, rest;
	if (str_split_once_str(str_from(uri), STR_LIT("://"), &scheme_s, &rest)) {
		utf8_to_lower(&scheme_b, scheme_s);
	} else {
		strbuf_append(&scheme_b, "", 0);
		rest = str_from(uri);
		while (str_starts_with(rest, STR_LIT("//"))) {
			rest = str_slice(rest, 2, rest.len);
		}
	}
	const char *scheme = strbuf_cstr(&scheme_b);
	ptrdiff_t slash = str_index(rest, '/');
	AUTOFREE char *host = str_dup(slash >= 0 ? str_slice(rest, 0, (size_t)slash) : rest);
	struct str path = slash >= 0 ? str_slice(rest, (size_t)slash, rest.len) : STR_EMPTY;
	AUTOFREE char *unix_path = xasprintf("%s%.*s", host, STR_FMT(path));

	struct listener_spec l = {.label = nullptr};
	if (cstr_eq(scheme, "ircs") || cstr_eq(scheme, "")) {
		l = (struct listener_spec){
			.kind = LISTENER_IRC, .addr = with_default_port(host, "6697"), .tls = true};
	} else if (cstr_eq(scheme, "irc") || cstr_eq(scheme, "irc+insecure")) {
		if (cstr_eq(scheme, "irc") && !is_localhost(host)) {
			return err_set(
				err, 0,
				"Plain-text IRC listening host must be localhost unless marked "
				"as insecure");
		}
		l = (struct listener_spec){.kind = LISTENER_IRC,
					   .addr = with_default_port(host, "6667")};
	} else if (cstr_eq(scheme, "unix")) {
		l = (struct listener_spec){
			.kind = LISTENER_IRC, .unix = true, .addr = xstrdup(unix_path)};
	} else if (cstr_eq(scheme, "unix+admin")) {
		const char *p = unix_path[0] != '\0' ? unix_path : CONFIG_DEFAULT_UNIX_ADMIN_PATH;
		l = (struct listener_spec){
			.kind = LISTENER_ADMIN, .unix = true, .addr = xstrdup(p)};
	} else if (cstr_eq(scheme, "wss") || cstr_eq(scheme, "https")) {
		l = (struct listener_spec){
			.kind = LISTENER_HTTP, .addr = with_default_port(host, "443"), .tls = true};
	} else if (cstr_eq(scheme, "ws") || cstr_eq(scheme, "ws+insecure") ||
		   cstr_eq(scheme, "http") || cstr_eq(scheme, "http+insecure")) {
		if ((cstr_eq(scheme, "ws") || cstr_eq(scheme, "http")) && !is_localhost(host)) {
			return err_set(
				err, 0,
				"Plain-text %s listening host must be localhost unless marked "
				"as insecure",
				cstr_eq(scheme, "ws") ? "WebSocket" : "HTTP");
		}
		l = (struct listener_spec){.kind = LISTENER_HTTP,
					   .addr = with_default_port(host, "80")};
	} else if (cstr_eq(scheme, "ws+unix") || cstr_eq(scheme, "http+unix")) {
		l = (struct listener_spec){
			.kind = LISTENER_HTTP, .unix = true, .addr = xstrdup(unix_path)};
	} else if (cstr_eq(scheme, "ident")) {
		l = (struct listener_spec){.kind = LISTENER_IDENT,
					   .addr = with_default_port(host, "113")};
	} else if (cstr_eq(scheme, "http+prometheus") ||
		   cstr_eq(scheme, "http+insecure+prometheus")) {
		char *h, *p;
		if (!split_host_port(str_from(host), &h, &p, nullptr)) {
			AUTOFREE char *q = utf8_debug_cstr(str_from(listen));
			return err_set(err, 0, "invalid host in URI %s: missing port in address",
				       q);
		}
		xfree(h);
		xfree(p);
		if (cstr_eq(scheme, "http+prometheus") && !is_localhost(host)) {
			return err_set(
				err, 0,
				"Prometheus listening host must be localhost unless marked as "
				"insecure");
		}
		l = (struct listener_spec){.kind = LISTENER_PROMETHEUS, .addr = xstrdup(host)};
	} else if (cstr_eq(scheme, "http+pprof")) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(listen));
		return err_set(err, 0, "failed to listen on %s: pprof is not supported", q);
	} else {
		AUTOFREE char *q = utf8_debug_cstr(str_from(listen));
		return err_set(err, 0, "failed to listen on %s: unsupported scheme", q);
	}
	l.label = xstrdup(listen);
	*out = l;
	return true;
}
