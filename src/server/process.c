// Process ownership: loop zero coordinates signals, listeners, maintenance
// and shutdown. Every other loop frees its own connections before stopping.
#include "server/process.h"

#include <signal.h>
#include <stdio.h>

#include "base/alloc.h"
#include "http/client.h"
#include "downstream/downstream.h"
#include "net/listener.h"
#include "server/admin.h"
#include "server/http.h"
#include "server/maintenance.h"
#include "version.h"

struct process {
	struct loop_pool loops;
	struct server server;
	struct db_thread *db;
	struct logger log;
	struct log_root log_root;
	struct tls_cert_store certificates;
	SSL_CTX *irc_tls, *http_tls;
	atomic_bool client_cert_auth;
	struct identd identd;
	VEC(struct listener *) listeners;
	struct maintenance *maintenance;
	uv_signal_t hup, term, interrupt;
	uv_timer_t shutdown;
	const char *config_path;
	struct server_config *loaded;
	struct err *error;
	bool loading, initialized, stopping;
};

static void configuration_loaded(void *data, struct server_config *config,
				 const struct err *error) {
	struct process *p = data;
	p->loading = false;
	if (p->stopping) {
		server_config_unref(config);
		return;
	}
	if (error != nullptr) {
		if (p->initialized) {
			log_info(&p->log, "failed to reload configuration: %s", err_msg(error));
		} else {
			bool failed = err_set(&p->error, 0, "failed to load configuration: %s",
					      err_msg(error));
			(void)failed;
		}
		return;
	}
	struct err *tls_error = nullptr;
	if (config->cfg.has_tls && !tls_cert_store_load(&p->certificates, config->cfg.tls_cert_path,
							config->cfg.tls_key_path, &tls_error)) {
		server_config_unref(config);
		if (p->initialized) {
			log_info(&p->log, "failed to reload TLS certificate: %s",
				 err_msg(tls_error));
			err_free(tls_error);
		} else {
			p->error = tls_error;
		}
		return;
	}
	atomic_store(&p->client_cert_auth, config->cfg.basic.client_cert_auth);
	if (p->initialized) {
		server_config_reload(&p->server, config);
		log_info(&p->log, "configuration reloaded");
	} else {
		p->loaded = config;
	}
}

static void close_inbox(void *data) {
	loop_close_inbox(data);
}
static void close_connections(void *data) {
	struct server *srv = data;
	conn_close_all();
	identd_close_all();
	atomic_fetch_sub(&srv->pending_work, 1);
}
static void check_shutdown(uv_timer_t *timer) {
	struct process *p = timer->data;
	if (p->loading || !server_is_idle(&p->server)) {
		return;
	}
	db_thread_stop(p->db);
	p->db = nullptr;
	server_finish(&p->server);
	uv_timer_stop(timer);
	uv_close((uv_handle_t *)timer, nullptr);
	for (unsigned i = 0; i < p->loops.n; i++) {
		loop_post(&p->loops.loops[i], close_inbox, nullptr, &p->loops.loops[i]);
	}
}
static void stop(struct process *p) {
	if (p->stopping) {
		return;
	}
	p->stopping = true;
	log_info(&p->log, "shutting down");
	for (size_t i = 0; i < p->listeners.len; i++) {
		listener_stop(p->listeners.data[i]);
	}
	vec_free(&p->listeners);
	maintenance_stop(p->maintenance);
	p->maintenance = nullptr;
	uv_signal_stop(&p->hup);
	uv_signal_stop(&p->term);
	uv_signal_stop(&p->interrupt);
	uv_close((uv_handle_t *)&p->hup, nullptr);
	uv_close((uv_handle_t *)&p->term, nullptr);
	uv_close((uv_handle_t *)&p->interrupt, nullptr);
	server_stop(&p->server);
	for (unsigned i = 0; i < p->loops.n; i++) {
		atomic_fetch_add(&p->server.pending_work, 1);
		loop_post(&p->loops.loops[i], close_connections, nullptr, &p->server);
	}
	uv_timer_start(&p->shutdown, check_shutdown, 0, 10);
}
static void on_signal(uv_signal_t *handle, int number) {
	struct process *p = handle->data;
	if (number == SIGHUP && !p->stopping) {
		if (!p->loading) {
			log_info(&p->log, "reloading configuration");
			p->loading = true;
			server_config_load(&p->loops.loops[0].uv, p->config_path,
					   configuration_loaded, p);
		}
	} else {
		stop(p);
	}
}

static bool trusted_proxy(const struct ipaddr *ip, void *data) {
	struct process *p = data;
	struct server_config *cfg = server_config_get(&p->server);
	bool trusted = ip != nullptr ? ipset_contains(&cfg->cfg.basic.accept_proxy_ips, *ip)
				     : cfg->cfg.basic.accept_proxy_unix;
	server_config_unref(cfg);
	return trusted;
}
static void serve_irc(struct conn *conn, const struct conn_info *info, void *data) {
	struct process *p = data;
	downstream_serve(&p->server, conn, info, nullptr);
}
static void serve_admin(struct conn *conn, const struct conn_info *info, void *data) {
	(void)info;
	struct process *p = data;
	admin_serve(&p->server, conn);
}
static void serve_http(struct conn *conn, const struct conn_info *info, bool prometheus,
		       void *data) {
	struct process *p = data;
	http_serve(&p->server, conn, info, prometheus);
}
static bool start_listeners(struct process *p, const strvec *extra) {
	struct server_config *cfg = server_config_get(&p->server);
	struct listener_handlers handlers = {.irc = serve_irc,
					     .admin = serve_admin,
					     .http = serve_http,
					     .identd = &p->identd,
					     .accept_proxy = trusted_proxy,
					     .data = p};
	size_t count = cfg->cfg.listen.len + extra->len;
	bool ok = true;
	for (size_t i = 0; ok && i < (count > 0 ? count : 1); i++) {
		const char *address = count == 0 ? ":6697"
				      : i < cfg->cfg.listen.len
					      ? cfg->cfg.listen.data[i]
					      : extra->data[i - cfg->cfg.listen.len];
		struct listener_spec spec;
		if (!parse_listen_uri(address, &spec, &p->error)) {
			ok = false;
			break;
		}
		SSL_CTX *tls = nullptr;
		if (spec.tls) {
			SSL_CTX **slot = spec.kind == LISTENER_HTTP ? &p->http_tls : &p->irc_tls;
			if (!cfg->cfg.has_tls) {
				ok = err_set(&p->error, 0, "TLS listener requires a tls directive");
			} else if (*slot == nullptr) {
				*slot = tls_server_ctx(&p->certificates,
						       spec.kind == LISTENER_HTTP ? "http/1.1"
										  : "irc",
						       nullptr, &p->error);
				ok = *slot != nullptr;
				if (ok) {
					tls_server_ctx_set_client_cert_auth(*slot,
									    &p->client_cert_auth);
				}
			}
			tls = *slot;
		}
		if (ok) {
			struct listener *listener = nullptr;
			uint16_t port;
			ok = listener_start(&p->loops, &spec, tls, &handlers, &p->log, &listener,
					    &port, &p->error);
			if (ok) {
				vec_push(&p->listeners, listener);
				log_info(&p->log, "server listening on \"%s\"", address);
			}
		}
		listener_spec_free(&spec);
	}
	server_config_unref(cfg);
	return ok;
}

int server_run(const char *config_path, const strvec *extra_listeners, bool debug) {
	struct process p = {.config_path = config_path};
	log_init(&p.log, &p.log_root);
	log_set_debug(&p.log, debug);
	log_info(&p.log, "starting soju %s", SOJU_VERSION);
	tls_cert_store_init(&p.certificates);
	identd_init(&p.identd);
	atomic_init(&p.client_cert_auth, false);
	loop_pool_init(&p.loops, loop_count_default());
	loop_set_current(&p.loops.loops[0]);
	uv_loop_t *zero = &p.loops.loops[0].uv;
	p.loading = true;
	server_config_load(zero, config_path, configuration_loaded, &p);
	while (p.loading) {
		uv_run(zero, UV_RUN_ONCE);
	}
	if (p.error != nullptr) {
		goto early_failure;
	}
	struct config *config = &p.loaded->cfg;
	struct db *db = nullptr;
	bool opened = config->has_turso
			      ? db_open_turso(&config->turso, nullptr, nullptr, &db, &p.error)
		      : cstr_eq(config->db_driver, "sqlite3")
			      ? db_open(config->db_source, &db, &p.error)
			      : err_set(&p.error, 0, "unsupported database driver: %s",
					config->db_driver);
	if (!opened) {
		goto early_failure;
	}
	if (!db_thread_start(zero, db, 0, &p.db, &p.error)) {
		db_close(db);
		goto early_failure;
	}
	server_init(&p.server, &p.loops, p.db);
	p.server.identd = &p.identd;
	log_set_debug(&p.server.log, debug);
	server_config_set(&p.server, p.loaded);
	p.loaded = nullptr;
	p.initialized = true;
	uv_signal_init(zero, &p.hup);
	uv_signal_init(zero, &p.term);
	uv_signal_init(zero, &p.interrupt);
	p.hup.data = p.term.data = p.interrupt.data = &p;
	uv_signal_start(&p.hup, on_signal, SIGHUP);
	uv_signal_start(&p.term, on_signal, SIGTERM);
	uv_signal_start(&p.interrupt, on_signal, SIGINT);
	uv_timer_init(zero, &p.shutdown);
	p.shutdown.data = &p;
	if (!start_listeners(&p, extra_listeners) || !server_start(&p.server, &p.error)) {
		log_info(&p.log, "%s", err_msg(p.error));
		stop(&p);
	} else {
		p.maintenance = maintenance_start(&p.server);
	}
	loop_pool_run(&p.loops);
	loop_pool_free(&p.loops);
	loop_set_current(nullptr);
	SSL_CTX_free(p.irc_tls);
	SSL_CTX_free(p.http_tls);
	tls_cert_store_finish(&p.certificates);
	identd_finish(&p.identd);
	log_free(&p.log);
	bool failed = p.error != nullptr;
	err_free(p.error);
	return failed ? 1 : 0;

early_failure:
	log_info(&p.log, "%s", err_msg(p.error));
	err_free(p.error);
	server_config_unref(p.loaded);
	http_client_cancel_all();
	for (unsigned i = 0; i < p.loops.n; i++) {
		loop_close_inbox(&p.loops.loops[i]);
		uv_run(&p.loops.loops[i].uv, UV_RUN_DEFAULT);
	}
	loop_pool_free(&p.loops);
	loop_set_current(nullptr);
	tls_cert_store_finish(&p.certificates);
	identd_finish(&p.identd);
	log_free(&p.log);
	return 1;
}
