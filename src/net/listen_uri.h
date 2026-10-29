// Listen URIs (soju-rs listen.rs parse_listen_uri)
#pragma once

#include "base/err.h"
#include "base/str.h"

enum listener_kind {
	LISTENER_IRC, // IRC over a stream (TCP, TLS, Unix socket)
	LISTENER_HTTP, // HTTP with /socket (WebSocket) and /icon
	LISTENER_IDENT,
	LISTENER_PROMETHEUS,
	LISTENER_ADMIN,
};

struct listener_spec {
	char *label; // the URI as configured
	enum listener_kind kind;
	bool unix; // addr is a Unix socket path, else a TCP host:port
	char *addr;
	bool tls;
};

[[nodiscard]] bool parse_listen_uri(const char *listen, struct listener_spec *out,
				    struct err **err);
void listener_spec_free(struct listener_spec *l);
