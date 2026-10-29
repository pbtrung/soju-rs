// Server metrics in the Prometheus text format (soju-rs metrics.rs)
#pragma once

#include "base/str.h"
#include "server/server.h"

// Renders all metrics; users is the current number of active users
void metrics_render(const struct server_metrics *m, size_t users, struct strbuf *out);
void server_render_metrics(struct server *srv, struct strbuf *out);
