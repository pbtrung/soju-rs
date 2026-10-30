#include "server/maintenance.h"

#include "base/alloc.h"
#include "user/user.h"

static constexpr uint64_t DAILY_MS = 24ULL * 60 * 60 * 1000;
static constexpr size_t EXPIRY_CHUNK = 2000;
struct maintenance {
	struct ref_node ref;
	struct server *srv;
	uv_timer_t timer;
	unsigned refs;
	// vacuum: this run vacuums local SQLite; vacuum_requested: the next one
	bool stopped, active, local, expire, initial, vacuum, vacuum_requested;
	struct timestamp before, inactive_before;
	bool check_inactive;
	size_t deleted, total;
	strvec inactive;
	struct err *error;
};

static void unref(struct maintenance *m) {
	if (--m->refs != 0) {
		return;
	}
	strvec_free(&m->inactive);
	err_free(m->error);
	ref_untrack(&m->ref);
	xfree(m);
}
static struct timestamp before_now(duration_ns duration) {
	struct timestamp now = timestamp_now();
	now.sec -= (int64_t)(duration / 1000000000);
	uint32_t fraction = (uint32_t)(duration % 1000000000);
	if (now.nsec < fraction) {
		now.sec--;
		now.nsec += 1000000000;
	}
	now.nsec -= fraction;
	return now;
}
struct disable_job {
	struct db_user record;
	struct err *error;
};
static void store_disabled(struct db *db, void *data) {
	struct disable_job *job = data;
	bool ok = db_store_user_keep_password(db, &job->record, &job->error);
	(void)ok;
}
static void disable_user(struct user *user, void *data) {
	struct timestamp *cutoff = data;
	bool inactive = !user->record.downstream_interacted_at.set ||
			timestamp_cmp(user->record.downstream_interacted_at.t, *cutoff) < 0;
	xfree(cutoff);
	if (user->clients.len > 0 || !user->record.enabled || !inactive) {
		return;
	}
	struct disable_job job = {0};
	db_user_copy(&job.record, &user->record);
	job.record.enabled = false;
	db_thread_call(user->srv->db, store_disabled, &job);
	if (job.error == nullptr) {
		user->record.enabled = false;
		for (size_t i = 0; i < user->networks.len; i++) {
			struct user_network *net = user->networks.data[i];
			upstream_free(net->upstream);
			net->upstream = nullptr;
			net->joined_generation = 0;
			user_notify_network(user, net);
		}
		log_info(&user->srv->log, "disabled inactive user \"%s\"", user->record.username);
	} else {
		log_info(&user->srv->log, "failed to disable inactive user: %s",
			 err_msg(job.error));
	}
	err_free(job.error);
	db_user_free(&job.record);
}
static void run_batch(struct db *db, void *data) {
	struct maintenance *m = data;
	m->deleted = 0;
	if (m->initial) {
		if (m->check_inactive &&
		    !db_list_inactive_usernames(db, m->inactive_before, &m->inactive, &m->error)) {
			return;
		}
	}
	if (!m->expire || atomic_load(&m->srv->shutting_down)) {
		return;
	}
	if (!db_delete_messages_before(db, m->before, EXPIRY_CHUNK, &m->deleted, &m->error)) {
		return;
	}
	m->total += m->deleted;
	if (m->deleted < EXPIRY_CHUNK && m->local) {
		bool ok = (m->total == 0 || db_optimize(db, &m->error)) &&
			  (!m->vacuum || db_vacuum(db, &m->error)) &&
			  db_shrink_memory(db, &m->error);
		(void)ok;
	}
}
static void tick(uv_timer_t *timer);
static void batch_done(void *data) {
	struct maintenance *m = data;
	for (size_t i = 0; !m->stopped && i < m->inactive.len; i++) {
		int64_t id;
		if (server_find_user(m->srv, m->inactive.data[i], &id)) {
			struct timestamp *cutoff = xmemdup(&m->inactive_before, sizeof(*cutoff));
			user_post(m->srv, id, disable_user, xfree, cutoff);
		}
	}
	strvec_free(&m->inactive);
	m->initial = false;
	if (m->error != nullptr) {
		log_info(&m->srv->log, "maintenance failed: %s", err_msg(m->error));
		err_free(m->error);
		m->error = nullptr;
		m->active = false;
	} else if (!m->expire || m->deleted < EXPIRY_CHUNK) {
		if (m->expire) {
			log_info(&m->srv->log, "deleted %zu expired messages", m->total);
		}
		m->active = false;
	}
	if (!m->stopped) {
		// A vacuum requested meanwhile runs right away
		uv_timer_start(&m->timer, tick,
			       m->active             ? 10
			       : m->vacuum_requested ? 0
						     : DAILY_MS,
			       0);
	}
	unref(m);
}
static void tick(uv_timer_t *timer) {
	struct maintenance *m = timer->data;
	if (m->stopped) {
		return;
	}
	if (!m->active) {
		struct server_config *cfg = server_config_get(m->srv);
		m->expire = cfg->cfg.basic.message_expiry != 0 &&
			    cfg->cfg.basic.msg_store_driver != MSG_STORE_MEMORY;
		m->check_inactive = cfg->cfg.basic.disable_inactive_users_delay != 0;
		m->local = cfg->cfg.basic.msg_store_driver == MSG_STORE_DB;
		m->before = before_now(cfg->cfg.basic.message_expiry);
		m->inactive_before = before_now(cfg->cfg.basic.disable_inactive_users_delay);
		server_config_unref(cfg);
		m->active = m->initial = true;
		m->vacuum = m->vacuum_requested;
		m->vacuum_requested = false;
		m->total = 0;
	}
	m->refs++;
	server_db(m->srv, run_batch, batch_done, m);
}
struct maintenance *maintenance_start(struct server *srv) {
	struct maintenance *m = xcalloc(1, sizeof(*m));
	REF_TRACK(&m->ref, "maintenance");
	m->srv = srv;
	m->refs = 1;
	uv_timer_init(&srv->loops->loops[0].uv, &m->timer);
	m->timer.data = m;
	// The startup expiry already ran, see maintenance_startup: this first
	// run checks inactive users
	uv_timer_start(&m->timer, tick, 0, 0);
	return m;
}

void maintenance_request_vacuum(struct maintenance *m) {
	if (m == nullptr || m->stopped) {
		return;
	}
	m->vacuum_requested = true;
	if (!m->active) {
		uv_timer_start(&m->timer, tick, 0, 0);
	}
}

struct startup_job {
	struct timestamp before;
	bool local;
	size_t total;
	double free_ratio;
	struct err *error;
};
static void startup_run(struct db *db, void *data) {
	struct startup_job *j = data;
	size_t deleted;
	do {
		if (!db_delete_messages_before(db, j->before, EXPIRY_CHUNK, &deleted, &j->error)) {
			return;
		}
		j->total += deleted;
	} while (deleted == EXPIRY_CHUNK);
	if (j->local) {
		bool ok = (j->total == 0 || db_optimize(db, &j->error)) &&
			  db_free_page_ratio(db, &j->free_ratio, &j->error) &&
			  db_vacuum(db, &j->error) && db_shrink_memory(db, &j->error);
		(void)ok;
	}
}

void maintenance_startup(struct server *srv) {
	struct server_config *cfg = server_config_get(srv);
	struct startup_job j = {
		.before = before_now(cfg->cfg.basic.message_expiry),
		.local = cfg->cfg.basic.msg_store_driver == MSG_STORE_DB,
	};
	bool expire = cfg->cfg.basic.message_expiry != 0 &&
		      cfg->cfg.basic.msg_store_driver != MSG_STORE_MEMORY;
	server_config_unref(cfg);
	if (!expire) {
		return;
	}
	log_info(&srv->log, "running message expiry");
	db_thread_call(srv->db, startup_run, &j);
	if (j.error != nullptr) {
		log_info(&srv->log, "failed to delete expired messages: %s", err_msg(j.error));
		err_free(j.error);
		return;
	}
	log_info(&srv->log, "deleted %zu expired messages", j.total);
	if (j.local) {
		log_info(&srv->log, "vacuumed db (%.0f%% of it was free)", j.free_ratio * 100);
	}
}
static void closed(uv_handle_t *handle) {
	unref(handle->data);
}
void maintenance_stop(struct maintenance *m) {
	if (m == nullptr || m->stopped) {
		return;
	}
	m->stopped = true;
	uv_timer_stop(&m->timer);
	uv_close((uv_handle_t *)&m->timer, closed);
}
