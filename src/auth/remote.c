// Remote authentication backends (soju-rs auth.rs): HTTP Basic checks and
// OAuth 2.0 token introspection, with server discovery
#include <ctype.h>
#include <string.h>
#include <yyjson.h>

#include "auth/auth.h"
#include "auth/internal.h"
#include "base/alloc.h"
#include "base/base64.h"
#include "base/utf8.h"
#include "http/client.h"
#include "net/url.h"

// reqwest client timeout of soju-rs
static constexpr uint64_t AUTH_HTTP_TIMEOUT_MS = 30000;

char *query_escape(struct str s) {
	struct strbuf out = STRBUF_INIT;
	for (size_t i = 0; i < s.len; i++) {
		unsigned char c = (unsigned char)s.p[i];
		if (isalnum(c) || c == '-' || c == '_' || c == '.' || c == '~') {
			strbuf_append_char(&out, (char)c);
		} else if (c == ' ') {
			strbuf_append_char(&out, '+');
		} else {
			strbuf_printf(&out, "%%%02X", c);
		}
	}
	return strbuf_take(&out);
}

static int hex_value(char c) {
	if (c >= '0' && c <= '9') {
		return c - '0';
	}
	if (c >= 'a' && c <= 'f') {
		return c - 'a' + 10;
	}
	if (c >= 'A' && c <= 'F') {
		return c - 'A' + 10;
	}
	return -1;
}

char *percent_decode(struct str s) {
	AUTO_STRBUF struct strbuf raw = STRBUF_INIT;
	for (size_t i = 0; i < s.len; i++) {
		// Exactly two hex digits, like soju-rs (which needs i + 2 < len)
		if (s.p[i] == '%' && i + 2 < s.len) {
			int hi = hex_value(s.p[i + 1]), lo = hex_value(s.p[i + 2]);
			if (hi >= 0 && lo >= 0) {
				strbuf_append_char(&raw, (char)(hi * 16 + lo));
				i += 2;
				continue;
			}
		}
		strbuf_append_char(&raw, s.p[i]);
	}
	struct strbuf out = STRBUF_INIT;
	utf8_lossy(&out, strbuf_str(&raw));
	return strbuf_take(&out);
}

void oauth2_ref(struct oauth2 *o) {
	atomic_fetch_add(&o->refs, 1);
}

void oauth2_unref(struct oauth2 *o) {
	if (o == nullptr || atomic_fetch_sub(&o->refs, 1) != 1) {
		return;
	}
	xfree(o->introspection_url);
	if (o->client_secret != nullptr) {
		explicit_bzero(o->client_secret, strlen(o->client_secret));
	}
	xfree(o->client_id);
	xfree(o->client_secret);
	xfree(o);
}

// Forwarded headers with the downstream address (soju-rs set_forwarded)
static void set_forwarded(struct http_client_request *req, const char *addr) {
	if (addr[0] == '\0') {
		return;
	}
	char *host = nullptr, *port = nullptr;
	if (!split_host_port(str_from(addr), &host, &port, nullptr)) {
		host = xstrdup("");
	}
	xfree(port);
	AUTOFREE char *q = utf8_debug_cstr(str_from(addr));
	http_client_request_header(req, "Forwarded", "for=%s", q);
	http_client_request_header(req, "X-Forwarded-For", "%s", host);
	xfree(host);
}

static void basic_auth(struct http_client_request *req, const char *user, const char *pass) {
	AUTO_STRBUF struct strbuf creds = STRBUF_INIT;
	strbuf_printf(&creds, "%s:%s", user, pass);
	AUTOFREE char *b64 = base64_encode_cstr(BASE64_STANDARD, creds.buf, creds.len);
	explicit_bzero(creds.buf, creds.len);
	http_client_request_header(req, "Authorization", "Basic %s", b64);
}

// A JSON string field: false on a type mismatch; *out is nullptr when
// missing or null
static bool json_string(yyjson_val *obj, const char *key, const char **out) {
	yyjson_val *v = yyjson_obj_get(obj, key);
	*out = nullptr;
	if (v == nullptr || yyjson_is_null(v)) {
		return true;
	}
	if (!yyjson_is_str(v)) {
		return false;
	}
	*out = yyjson_get_str(v);
	return true;
}

// A request on behalf of an authentication, counted as server work
struct remote_job {
	struct server *srv;
	char *username; // PLAIN
	auth_done_fn plain_done;
	auth_bearer_done_fn bearer_done;
	void *data;
};

static void remote_job_free(struct remote_job *j) {
	atomic_fetch_sub(&j->srv->pending_work, 1);
	xfree(j->username);
	xfree(j);
}

[[gnu::format(printf, 3, 4)]] static void job_fail(struct remote_job *j, bool invalid,
						   const char *fmt, ...) {
	struct auth_error e = {
		.external = xstrdup(invalid ? "Invalid credentials" : "Authentication failed")};
	struct strbuf sb = STRBUF_INIT;
	va_list ap;
	va_start(ap, fmt);
	strbuf_vprintf(&sb, fmt, ap);
	va_end(ap);
	e.internal = strbuf_take(&sb);
	if (j->plain_done != nullptr) {
		j->plain_done(j->data, &e);
	} else {
		j->bearer_done(j->data, nullptr, &e);
	}
	auth_error_free(&e);
	remote_job_free(j);
}

static void http_auth_done(void *data, const struct http_response *resp, const struct err *err) {
	struct remote_job *j = data;
	if (resp == nullptr) {
		job_fail(j, false, "failed to send HTTP auth request: %s", err_msg(err));
		return;
	}
	if (resp->status == 403) {
		job_fail(j, true, "HTTP auth server returned forbidden");
		return;
	}
	if (resp->status != 200) {
		AUTO_STRBUF struct strbuf st = STRBUF_INIT;
		http_status_text(&st, resp->status);
		job_fail(j, false, "HTTP auth error: %s", strbuf_cstr(&st));
		return;
	}
	j->plain_done(j->data, nullptr);
	remote_job_free(j);
}

static struct remote_job *new_job(struct server *srv) {
	struct remote_job *j = xcalloc(1, sizeof(*j));
	j->srv = srv;
	atomic_fetch_add(&srv->pending_work, 1);
	return j;
}

// Requests aren't started once the server stops: they would only be
// cancelled
static bool stopping(struct remote_job *j) {
	if (!atomic_load(&j->srv->shutting_down)) {
		return false;
	}
	job_fail(j, false, "server is shutting down");
	return true;
}

void auth_plain_http(struct server *srv, const char *url, const char *username,
		     const char *password, const char *remote_addr, auth_done_fn done, void *data) {
	struct remote_job *j = new_job(srv);
	j->plain_done = done;
	j->data = data;
	if (stopping(j)) {
		return;
	}
	struct http_client_request req;
	http_client_request_init(&req, "POST", url);
	req.timeout_ms = AUTH_HTTP_TIMEOUT_MS;
	req.log = &srv->log;
	basic_auth(&req, username, password);
	set_forwarded(&req, remote_addr);
	http_client_send(&loop_current()->uv, &req, http_auth_done, j);
	http_client_request_free(&req);
}

static void introspect_done(void *data, const struct http_response *resp, const struct err *err) {
	struct remote_job *j = data;
	if (resp == nullptr) {
		job_fail(j, false, "failed to send OAuth 2.0 introspection request: %s",
			 err_msg(err));
		return;
	}
	if (resp->status != 200) {
		AUTO_STRBUF struct strbuf st = STRBUF_INIT;
		http_status_text(&st, resp->status);
		job_fail(j, false, "OAuth 2.0 introspection error: %s", strbuf_cstr(&st));
		return;
	}
	yyjson_read_err rerr;
	yyjson_doc *doc = yyjson_read_opts(resp->body.buf, resp->body.len, 0, nullptr, &rerr);
	yyjson_val *root = doc != nullptr ? yyjson_doc_get_root(doc) : nullptr;
	yyjson_val *active = root != nullptr ? yyjson_obj_get(root, "active") : nullptr;
	const char *username = nullptr;
	if (root == nullptr || !yyjson_is_obj(root) ||
	    (active != nullptr && !yyjson_is_bool(active) && !yyjson_is_null(active)) ||
	    !json_string(root, "username", &username)) {
		job_fail(j, false, "failed to decode OAuth 2.0 introspection response: %s",
			 doc == nullptr ? rerr.msg : "invalid type");
		yyjson_doc_free(doc);
		return;
	}
	if (active == nullptr || !yyjson_get_bool(active)) {
		yyjson_doc_free(doc);
		job_fail(j, true, "invalid access token");
		return;
	}
	if (username == nullptr || username[0] == '\0') {
		// We really need the username here, otherwise an OAuth 2.0 user
		// can impersonate any other user.
		yyjson_doc_free(doc);
		job_fail(j, false, "missing username in OAuth 2.0 introspection response");
		return;
	}
	if (j->plain_done != nullptr) {
		if (!cstr_eq(j->username, username)) {
			AUTOFREE char *q = utf8_debug_cstr(str_from(username));
			yyjson_doc_free(doc);
			job_fail(j, true, "username mismatch (OAuth 2.0 server returned %s)", q);
			return;
		}
		j->plain_done(j->data, nullptr);
	} else {
		j->bearer_done(j->data, username, nullptr);
	}
	yyjson_doc_free(doc);
	remote_job_free(j);
}

static void introspect(struct remote_job *j, const struct oauth2 *o, const char *token,
		       const char *remote_addr) {
	if (stopping(j)) {
		return;
	}
	struct http_client_request req;
	http_client_request_init(&req, "POST", o->introspection_url);
	req.timeout_ms = AUTH_HTTP_TIMEOUT_MS;
	req.log = &j->srv->log;
	http_client_request_header(&req, "Content-Type", "application/x-www-form-urlencoded");
	http_client_request_header(&req, "Accept", "application/json");
	set_forwarded(&req, remote_addr);
	if (o->client_id[0] != '\0') {
		AUTOFREE char *id = query_escape(str_from(o->client_id));
		AUTOFREE char *secret = query_escape(str_from(o->client_secret));
		basic_auth(&req, id, secret);
	}
	AUTOFREE char *escaped = query_escape(str_from(token));
	strbuf_printf(&req.body, "token=%s", escaped);
	http_client_send(&loop_current()->uv, &req, introspect_done, j);
	http_client_request_free(&req);
}

void auth_plain_oauth2(struct server *srv, const struct oauth2 *o, const char *username,
		       const char *password, const char *remote_addr, auth_done_fn done,
		       void *data) {
	struct remote_job *j = new_job(srv);
	j->plain_done = done;
	j->data = data;
	j->username = xstrdup(username);
	introspect(j, o, password, remote_addr);
}

void auth_oauth_bearer(struct server *srv, const char *token, const char *remote_addr,
		       auth_bearer_done_fn done, void *data) {
	struct server_config *cfg = server_config_get(srv);
	if (cfg->oauth2 == nullptr) {
		server_config_unref(cfg);
		struct auth_error e = {.internal = xstrdup("SASL OAUTHBEARER not supported"),
				       .external = xstrdup("Authentication failed")};
		done(data, nullptr, &e);
		auth_error_free(&e);
		return;
	}
	struct remote_job *j = new_job(srv);
	j->bearer_done = done;
	j->data = data;
	introspect(j, cfg->oauth2, token, remote_addr);
	server_config_unref(cfg);
}
