// Authentication backends (soju-rs auth.rs). Internal PLAIN authentication
// checks the user database; HTTP and OAuth 2.0 come with phase 9.
#pragma once

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
