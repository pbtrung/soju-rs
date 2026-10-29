// Client addresses forwarded by trusted HTTP proxies, client certificates
// in headers, and the Origin check of WebSocket requests (soju-rs
// listen.rs)
#pragma once

#include "base/err.h"
#include "base/str.h"
#include "base/vec.h"

// The values of one HTTP header, in order (one per header line)
struct header_values {
	const struct str *v;
	size_t n;
};

// Parses a Forwarded header (RFC 7239) if present (forwarded.n > 0: its
// first value), or else X-Forwarded-For. Returns the client address the
// proxy forwarded (a new string), or nullptr.
[[nodiscard]] char *parse_forwarded(struct header_values forwarded,
				    struct header_values x_forwarded_for);
// The last address of X-Forwarded-For, the one added by the trusted proxy
[[nodiscard]] char *parse_x_forwarded_for(struct header_values x_forwarded_for);
// ":<base64>:" (a structured-field byte sequence); appends the bytes to out
[[nodiscard]] bool parse_client_cert(struct str s, struct strbuf *out);
// Checks the Origin header of a WebSocket request, like
// github.com/coder/websocket. origin and host: nullptr when missing.
[[nodiscard]] bool authenticate_origin(const char *origin, const char *host, const strvec *patterns,
				       struct err **err);
