// HTTP listeners (soju-rs listen.rs serve_http): WebSocket connections on
// /socket, the server icon on /icon, and Prometheus metrics
#pragma once

#include "net/listener.h"
#include "server/server.h"

// How long a client may take to send the headers of a request, including
// the first one
static constexpr uint64_t HTTP_HEADER_READ_TIMEOUT_MS = 30000;

// Serves HTTP requests on c (raw framing; takes the reference)
void http_serve(struct server *srv, struct conn *c, const struct conn_info *info, bool prometheus);
