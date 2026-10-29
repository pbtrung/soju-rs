// Authentication backends (soju-rs auth.rs): internal PLAIN authentication
// against the user database, an HTTP endpoint checking Basic credentials,
// and OAuth 2.0 token introspection (RFC 7662) with server discovery (RFC
// 8414), for OAUTHBEARER and PLAIN with a token as password.
#pragma once

#include <uv.h>

#include "config/config.h"
#include "server/server.h"

// The error of a failed authentication: the internal message is logged,
// the external one is safe to send to the user ("Invalid credentials" when
// the credentials were wrong, "Authentication failed" otherwise)
struct auth_error {
	char *internal;
	char *external;
};

void auth_error_free(struct auth_error *e);

// Called on the loop which started the authentication; err is nullptr on
// success
typedef void (*auth_done_fn)(void *data, const struct auth_error *err);

// Checks PLAIN credentials with the configured backend
void auth_plain(struct server *srv, const char *username, const char *password,
		const char *remote_addr, auth_done_fn done, void *data);

// Checks an OAUTHBEARER token: username is the user the OAuth 2.0 server
// returned, nullptr on error
typedef void (*auth_bearer_done_fn)(void *data, const char *username, const struct auth_error *err);
void auth_oauth_bearer(struct server *srv, const char *token, const char *remote_addr,
		       auth_bearer_done_fn done, void *data);

// A discovered OAuth 2.0 server (shared by configuration snapshots)
struct oauth2 {
	atomic_uint refs;
	char *introspection_url;
	char *client_id, *client_secret;
};

void oauth2_ref(struct oauth2 *o);
void oauth2_unref(struct oauth2 *o);

// The authenticators of the auth directives (soju-rs
// Authenticator::from_config); OAuth 2.0 servers are discovered first
struct auth_settings {
	enum plain_auth plain;
	char *http_url; // PLAIN_AUTH_HTTP
	struct oauth2 *oauth2; // OAUTHBEARER, and PLAIN_AUTH_OAUTH2
};

void auth_settings_free(struct auth_settings *s);
// Called on the loop (synchronously without OAuth 2.0 server to discover);
// s is taken (nullptr on error)
typedef void (*auth_settings_done_fn)(void *data, struct auth_settings *s, const struct err *err);
void auth_settings_load(uv_loop_t *loop, const struct config *cfg, auth_settings_done_fn done,
			void *data);

// Go's url.QueryEscape
[[nodiscard]] char *query_escape(struct str s);
// Decodes %XX escapes with exactly two hex digits, lossily as UTF-8
[[nodiscard]] char *percent_decode(struct str s);
