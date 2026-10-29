// Server metrics in the Prometheus text format (soju-rs metrics.rs)
#include "server/metrics.h"

#include "base/map.h"

static void gauge(struct strbuf *out, const char *name, const char *help, long long v) {
	strbuf_printf(out, "# HELP %s %s\n# TYPE %s gauge\n%s %lld\n", name, help, name, name, v);
}

static void counter(struct strbuf *out, const char *name, const char *help, unsigned long long v) {
	strbuf_printf(out, "# HELP %s %s\n# TYPE %s counter\n%s %llu\n", name, help, name, name, v);
}

void metrics_render(const struct server_metrics *m, size_t users, struct strbuf *out) {
	gauge(out, "soju_users_active", "Current number of active users", (long long)users);
	gauge(out, "soju_downstreams_active", "Current number of downstream connections",
	      (long long)atomic_load(&m->downstreams));
	gauge(out, "soju_upstreams_active", "Current number of upstream connections",
	      (long long)atomic_load(&m->upstreams));
	counter(out, "soju_upstream_out_messages_total",
		"Total number of outgoing messages sent to upstream servers",
		atomic_load(&m->upstream_out_messages_total));
	counter(out, "soju_upstream_in_messages_total",
		"Total number of incoming messages received from upstream servers",
		atomic_load(&m->upstream_in_messages_total));
	counter(out, "soju_downstream_out_messages_total",
		"Total number of outgoing messages sent to downstream clients",
		atomic_load(&m->downstream_out_messages_total));
	counter(out, "soju_downstream_in_messages_total",
		"Total number of incoming messages received from downstream clients",
		atomic_load(&m->downstream_in_messages_total));
	counter(out, "soju_upstream_connect_errors_total",
		"Total number of upstream connection errors",
		atomic_load(&m->upstream_connect_errors_total));
	// soju-rs counts panics of its tasks; the C port has none
	counter(out, "soju_worker_panics_total", "Total number of panics in worker goroutines",
		atomic_load(&m->worker_panics_total));
}

void server_render_metrics(struct server *srv, struct strbuf *out) {
	pthread_mutex_lock(&srv->users_mu);
	size_t users = strmap_len(&srv->users);
	pthread_mutex_unlock(&srv->users_mu);
	metrics_render(&srv->metrics, users, out);
}
