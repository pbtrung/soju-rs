// OAuth 2.0 server discovery (RFC 8414) and the authenticators of the auth
// directives (soju-rs auth.rs OAuth2::new, Authenticator::from_config)
#include <string.h>
#include <yyjson.h>

#include "auth/auth.h"
#include "base/alloc.h"
#include "http/client.h"
#include "net/url.h"

// soju-rs bounds the discovery to 10 s
static constexpr uint64_t OAUTH2_DISCOVERY_TIMEOUT_MS = 10000;

void auth_settings_free(struct auth_settings *s) {
	if (s == nullptr) {
		return;
	}
	xfree(s->http_url);
	oauth2_unref(s->oauth2);
	xfree(s);
}

struct discovery {
	struct auth_settings *settings;
	struct oauth2 *o;
	auth_settings_done_fn done;
	void *data;
};

static void discovery_finish(struct discovery *d, const char *err) {
	if (err != nullptr) {
		struct err e = {.msg = (char *)(uintptr_t)err};
		auth_settings_free(d->settings);
		oauth2_unref(d->o);
		d->done(d->data, nullptr, &e);
	} else {
		struct auth_settings *s = d->settings;
		s->oauth2 = d->o;
		if (s->plain == PLAIN_AUTH_NONE) {
			s->plain = PLAIN_AUTH_OAUTH2;
		}
		d->done(d->data, s, nullptr);
	}
	xfree(d);
}

static bool has_method(yyjson_val *methods, const char *name) {
	size_t i, n;
	yyjson_val *v;
	yyjson_arr_foreach(methods, i, n, v) {
		if (yyjson_is_str(v) && cstr_eq(yyjson_get_str(v), name)) {
			return true;
		}
	}
	return false;
}

// The server metadata: the introspection endpoint, after soju-rs's checks
static void discovered(struct discovery *d, yyjson_val *root) {
	yyjson_val *issuer = yyjson_obj_get(root, "issuer");
	yyjson_val *endpoint = yyjson_obj_get(root, "introspection_endpoint");
	yyjson_val *methods = yyjson_obj_get(root, "introspection_endpoint_auth_methods_supported");
	if ((issuer != nullptr && !yyjson_is_str(issuer)) ||
	    (endpoint != nullptr && !yyjson_is_str(endpoint)) ||
	    (methods != nullptr && !yyjson_is_arr(methods) && !yyjson_is_null(methods))) {
		discovery_finish(d,
				 "OAuth 2.0 discovery failed: failed to decode response: invalid "
				 "type");
		return;
	}
	if (issuer == nullptr || yyjson_get_len(issuer) == 0) {
		discovery_finish(d, "OAuth 2.0 discovery failed: missing issuer in response");
		return;
	}
	if (endpoint == nullptr || yyjson_get_len(endpoint) == 0) {
		discovery_finish(d, "OAuth 2.0 server doesn't support token introspection");
		return;
	}
	if (yyjson_is_arr(methods)) {
		bool none = has_method(methods, "none"),
		     basic = has_method(methods, "client_secret_basic");
		if (d->o->client_id[0] == '\0' && !none) {
			discovery_finish(
				d, "OAuth 2.0 server requires authentication for introspection");
			return;
		}
		if (d->o->client_id[0] != '\0' && !basic) {
			discovery_finish(
				d, "OAuth 2.0 server doesn't support Basic HTTP authentication "
				   "for introspection");
			return;
		}
	}
	d->o->introspection_url = xstrdup(yyjson_get_str(endpoint));
	discovery_finish(d, nullptr);
}

static void discovery_done(void *data, const struct http_response *resp, const struct err *err) {
	struct discovery *d = data;
	AUTO_STRBUF struct strbuf msg = STRBUF_INIT;
	if (resp == nullptr) {
		if (str_ends_with(str_from(err_msg(err)), STR_LIT("operation timed out"))) {
			strbuf_append_cstr(&msg, "context deadline exceeded");
		} else {
			strbuf_printf(&msg, "failed to send request: %s", err_msg(err));
		}
	} else if (resp->status != 200) {
		strbuf_append_cstr(&msg, "OAuth 2.0 HTTP error: ");
		http_status_text(&msg, resp->status);
	} else {
		yyjson_read_err rerr;
		yyjson_doc *doc =
			yyjson_read_opts(resp->body.buf, resp->body.len, 0, nullptr, &rerr);
		yyjson_val *root = doc != nullptr ? yyjson_doc_get_root(doc) : nullptr;
		if (root != nullptr && yyjson_is_obj(root)) {
			discovered(d, root);
			yyjson_doc_free(doc);
			return;
		}
		strbuf_printf(&msg, "failed to decode response: %s",
			      doc == nullptr ? rerr.msg : "invalid type");
		yyjson_doc_free(doc);
	}
	AUTOFREE char *full = xasprintf("OAuth 2.0 discovery failed: %s", strbuf_cstr(&msg));
	discovery_finish(d, full);
}

// Discovers the OAuth 2.0 server of an auth URL
static void discover(uv_loop_t *loop, struct discovery *d, const char *auth_url) {
	struct url u;
	struct err *e = nullptr;
	if (!url_parse(str_from(auth_url), &u, &e)) {
		AUTOFREE char *msg =
			xasprintf("failed to parse OAuth 2.0 server URL: %s", err_msg(e));
		err_free(e);
		discovery_finish(d, msg);
		return;
	}
	struct oauth2 *o = xcalloc(1, sizeof(*o));
	atomic_init(&o->refs, 1);
	d->o = o;
	// Client credentials in the URL's userinfo
	struct str rest = STR_EMPTY, scheme, authority, path, userinfo, id, secret;
	(void)str_split_once_str(str_from(auth_url), STR_LIT("://"), &scheme, &rest);
	authority = rest;
	(void)str_split_once(rest, '/', &authority, &path);
	ptrdiff_t at = str_rindex(authority, '@');
	if (at >= 0) {
		userinfo = str_slice(authority, 0, (size_t)at);
		if (str_split_once(userinfo, ':', &id, &secret)) {
			o->client_id = percent_decode(id);
			o->client_secret = percent_decode(secret);
		} else {
			o->client_id = percent_decode(userinfo);
		}
	}
	if (o->client_id == nullptr) {
		o->client_id = xstrdup("");
	}
	if (o->client_secret == nullptr) {
		o->client_secret = xstrdup("");
	}
	AUTO_STRBUF struct strbuf url = STRBUF_INIT;
	strbuf_printf(&url, "%s://%s/.well-known/oauth-authorization-server%s", u.scheme, u.host,
		      u.path);
	while (url.len > 0 && url.buf[url.len - 1] == '/') {
		strbuf_truncate(&url, url.len - 1);
	}
	url_free(&u);
	struct http_client_request req;
	http_client_request_init(&req, "GET", strbuf_cstr(&url));
	req.timeout_ms = OAUTH2_DISCOVERY_TIMEOUT_MS;
	http_client_request_header(&req, "Accept", "application/json");
	http_client_send(loop, &req, discovery_done, d);
	http_client_request_free(&req);
}

void auth_settings_load(uv_loop_t *loop, const struct config *cfg, auth_settings_done_fn done,
			void *data) {
	struct auth_settings *s = xcalloc(1, sizeof(*s));
	const char *oauth_url = nullptr;
	const char *err = nullptr;
	for (size_t i = 0; i < cfg->auth.len && err == nullptr; i++) {
		const struct config_auth *a = &cfg->auth.data[i];
		enum plain_auth plain = PLAIN_AUTH_NONE;
		switch (a->driver) {
		case AUTH_INTERNAL:
			plain = PLAIN_AUTH_INTERNAL;
			break;
		case AUTH_HTTP:
			plain = PLAIN_AUTH_HTTP;
			break;
		case AUTH_OAUTH2:
			if (oauth_url != nullptr) {
				err = "failed to load authenticators: multiple OAuth "
				      "authentication "
				      "methods specified";
			}
			oauth_url = a->source;
			break;
		case AUTH_PAM:
			err = "failed to create authenticator pam: PAM support is disabled";
			break;
		}
		if (plain != PLAIN_AUTH_NONE) {
			if (s->plain != PLAIN_AUTH_NONE) {
				err = "failed to load authenticators: multiple plain "
				      "authentication "
				      "methods specified";
			}
			s->plain = plain;
			if (plain == PLAIN_AUTH_HTTP) {
				s->http_url = xstrdup(a->source);
			}
		}
	}
	if (s->http_url == nullptr) {
		s->http_url = xstrdup("");
	}
	struct discovery *d = xcalloc(1, sizeof(*d));
	*d = (struct discovery){.settings = s, .done = done, .data = data};
	if (err != nullptr) {
		discovery_finish(d, err);
	} else if (oauth_url != nullptr) {
		discover(loop, d, oauth_url);
	} else {
		discovery_finish(d, nullptr);
	}
}
