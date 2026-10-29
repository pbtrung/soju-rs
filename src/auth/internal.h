// Internals of the authentication backends (auth/*.c)
#pragma once

#include "auth/auth.h"

// PLAIN through an HTTP endpoint checking Basic credentials
void auth_plain_http(struct server *srv, const char *url, const char *username,
		     const char *password, const char *remote_addr, auth_done_fn done, void *data);
// PLAIN with an OAuth 2.0 token as password: the token's user must be
// username
void auth_plain_oauth2(struct server *srv, const struct oauth2 *o, const char *username,
		       const char *password, const char *remote_addr, auth_done_fn done,
		       void *data);
