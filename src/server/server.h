// The server: configuration, database, loops and process-wide state shared
// by users and connections (soju-rs server.rs). Only what the ported
// phases use so far; later phases add users, networks and services.
#pragma once

#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>

#include "base/cpuwork.h"
#include "base/log.h"
#include "config/config.h"
#include "db/dbthread.h"
#include "irc/message.h"
#include "net/loops.h"
#include "base/map.h"

enum plain_auth {
	PLAIN_AUTH_NONE,
	PLAIN_AUTH_INTERNAL,
	PLAIN_AUTH_HTTP, // phase 9
	PLAIN_AUTH_OAUTH2, // phase 9: PLAIN with an OAuth 2.0 token
};

// The run-time configuration, reloadable on SIGHUP: an immutable snapshot
// shared by the loops (reference counted)
struct server_config {
	atomic_uint refs;
	struct config cfg; // the parsed file (basic is the reloadable part)
	char *icon_path;
	char *icon_url;
	char *motd;
	enum plain_auth plain_auth;
	char *http_auth_url;
	bool oauth_bearer; // phase 9
};

// A snapshot with soju-rs's defaults (hostname "localhost", internal auth)
[[nodiscard]] struct server_config *server_config_default(void);
void server_config_unref(struct server_config *c);

struct server_metrics {
	atomic_int_fast64_t downstreams, upstreams;
	atomic_uint_fast64_t upstream_out_messages_total, upstream_in_messages_total;
	atomic_uint_fast64_t downstream_out_messages_total, downstream_in_messages_total;
	atomic_uint_fast64_t upstream_connect_errors_total;
	atomic_uint_fast64_t worker_panics_total; // always 0: kept for the metric
};

struct server {
	struct logger log;
	struct log_root log_root;
	struct loop_pool *loops; // borrowed
	struct db_thread *db; // borrowed
	struct cpu_work_pool *cpu; // on loop 0
	struct server_metrics metrics;
	pthread_mutex_t config_mu;
	struct server_config *config;
	atomic_uint_fast64_t next_downstream_id;
	atomic_bool shutting_down;
	atomic_size_t pending_work;
	pthread_mutex_t users_mu;
	struct strmap deleted_users; // stopped IDs; reject stale authentication/start completions
	struct strmap users; // username -> owned int64_t user ID; no actor pointers
};

[[nodiscard]] bool server_start(struct server *srv, struct err **err);
void server_stop(struct server *srv);
// Thread-safe: stop has removed all actors/connections and their jobs are done.
[[nodiscard]] bool server_is_idle(struct server *srv);
[[nodiscard]] bool server_find_user(struct server *srv, const char *username, int64_t *id);

[[nodiscard]] bool server_user_deleted(struct server *srv, int64_t id);
void server_mark_user_deleted(struct server *srv, int64_t id);

void server_init(struct server *srv, struct loop_pool *loops, struct db_thread *db);
void server_finish(struct server *srv);
// The current configuration (a reference: server_config_unref)
[[nodiscard]] struct server_config *server_config_get(struct server *srv);
// Replaces the configuration (takes the reference)
void server_config_set(struct server *srv, struct server_config *c);
// Reloads mutable settings, retaining the message-store selection (takes c).
void server_config_reload(struct server *srv, struct server_config *c);
typedef void (*server_user_done_fn)(void *data, int64_t id, const struct err *err);
void server_get_or_create_user(struct server *srv, const char *username, server_user_done_fn done,
			       void *data);
// The server's prefix: its hostname (a new message prefix name)
[[nodiscard]] char *server_hostname(struct server *srv);
// Sets the prefix of msg to the server's
void server_set_prefix(struct server *srv, struct irc_msg *msg);

// Runs run on the DB thread, then done on the calling loop
void server_db(struct server *srv, db_job_fn run, db_done_fn done, void *data);
// Runs work through the CPU-job limiter, then done(data, err) on the
// calling loop; err is set if the job was refused ("server busy")
typedef void (*server_cpu_done_fn)(void *data, const char *err);
void server_cpu(struct server *srv, cpu_work_fn work, server_cpu_done_fn done, void *data);
