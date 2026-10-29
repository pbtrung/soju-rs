#include "server/server.h"

#include "auth/auth.h"
#include "base/alloc.h"
#include "http/client.h"
#include "user/user.h"

struct server_config *server_config_default(void) {
	struct server_config *c = xcalloc(1, sizeof(*c));
	atomic_init(&c->refs, 1);
	config_defaults(&c->cfg);
	xfree(c->cfg.basic.hostname);
	c->cfg.basic.hostname = xstrdup("localhost");
	c->icon_path = xstrdup("");
	c->icon_url = xstrdup("");
	c->motd = xstrdup("");
	c->plain_auth = PLAIN_AUTH_INTERNAL;
	c->http_auth_url = xstrdup("");
	return c;
}

void server_config_unref(struct server_config *c) {
	if (c == nullptr || atomic_fetch_sub(&c->refs, 1) != 1) {
		return;
	}
	config_free(&c->cfg);
	xfree(c->icon_path);
	xfree(c->icon_url);
	xfree(c->motd);
	xfree(c->http_auth_url);
	oauth2_unref(c->oauth2);
	xfree(c);
}

void server_init(struct server *srv, struct loop_pool *loops, struct db_thread *db) {
	*srv = (struct server){.loops = loops, .db = db};
	log_init(&srv->log, &srv->log_root);
	pthread_mutex_init(&srv->config_mu, nullptr);
	pthread_mutex_init(&srv->users_mu, nullptr);
	strmap_init(&srv->users, xfree);
	strmap_init(&srv->deleted_users, nullptr);
	srv->config = server_config_default();
	srv->cpu = cpu_work_pool_new(&loops->loops[0].uv, 0);
	atomic_init(&srv->next_downstream_id, 1);
}

void server_finish(struct server *srv) {
	webpush_sender_free(srv->web_push);
	cpu_work_pool_free(srv->cpu);
	server_config_unref(srv->config);
	pthread_mutex_destroy(&srv->config_mu);
	strmap_free(&srv->users);
	strmap_free(&srv->deleted_users);
	pthread_mutex_destroy(&srv->users_mu);
	log_free(&srv->log);
}

// The VAPID key pair, generated on first start (soju-rs
// load_web_push_config)
struct web_push_load {
	struct webpush_sender *sender;
	bool generated;
	struct err *err;
};

static void load_web_push_config(struct db *db, void *data) {
	struct web_push_load *j = data;
	db_webpush_config_vec configs = {0};
	struct err *e = nullptr;
	if (!db_list_webpush_configs(db, &configs, &e)) {
		err_set(&j->err, 0, "failed to list Web push configs: %s", err_msg(e));
		err_free(e);
		return;
	}
	struct db_webpush_config c;
	db_webpush_config_init(&c);
	if (configs.len == 0) {
		j->generated = true;
		xfree(c.vapid_private);
		xfree(c.vapid_public);
		webpush_generate_vapid_keys(&c.vapid_private, &c.vapid_public);
		if (!db_store_webpush_config(db, &c, &e)) {
			err_set(&j->err, 0, "failed to store Web push config: %s", err_msg(e));
			err_free(e);
		}
	} else if (configs.len == 1) {
		db_webpush_config_free(&c);
		c = configs.data[0];
		configs.len = 0;
	} else {
		err_set(&j->err, 0, "expected zero or one Web push config, got %zu", configs.len);
	}
	if (j->err == nullptr) {
		j->sender = xmalloc(sizeof(*j->sender));
		*j->sender = (struct webpush_sender){.vapid_public = xstrdup(c.vapid_public),
						     .vapid_private = xstrdup(c.vapid_private)};
	}
	db_webpush_config_free(&c);
	db_webpush_config_vec_free(&configs);
}

bool server_start(struct server *srv, struct err **err) {
	struct web_push_load j = {0};
	db_thread_call(srv->db, load_web_push_config, &j);
	if (j.err != nullptr) {
		if (err != nullptr) {
			*err = j.err;
		} else {
			err_free(j.err);
		}
		return false;
	}
	if (j.generated) {
		log_info(&srv->log, "generating Web push VAPID key pair");
	}
	srv->web_push = j.sender;
	return user_server_start(srv, err);
}

static void cancel_http(void *data) {
	(void)data;
	http_client_cancel_all();
}

void server_stop(struct server *srv) {
	atomic_store(&srv->shutting_down, true);
	user_server_stop(srv);
	// Authentication and Web Push requests in flight fail at once
	for (unsigned i = 0; i < srv->loops->n; i++) {
		loop_post(&srv->loops->loops[i], cancel_http, nullptr, nullptr);
	}
}

bool server_is_idle(struct server *srv) {
	pthread_mutex_lock(&srv->users_mu);
	bool empty = strmap_len(&srv->users) == 0;
	pthread_mutex_unlock(&srv->users_mu);
	return empty && atomic_load(&srv->pending_work) == 0 &&
	       atomic_load(&srv->metrics.downstreams) == 0;
}

bool server_find_user(struct server *srv, const char *username, int64_t *id) {
	pthread_mutex_lock(&srv->users_mu);
	int64_t *found = strmap_get(&srv->users, username);
	if (found != nullptr) {
		*id = *found;
	}
	pthread_mutex_unlock(&srv->users_mu);
	return found != nullptr;
}

bool server_user_deleted(struct server *srv, int64_t id) {
	AUTOFREE char *key = xasprintf("%lld", (long long)id);
	pthread_mutex_lock(&srv->users_mu);
	bool deleted = strmap_has(&srv->deleted_users, key);
	pthread_mutex_unlock(&srv->users_mu);
	return deleted;
}
void server_mark_user_deleted(struct server *srv, int64_t id) {
	AUTOFREE char *key = xasprintf("%lld", (long long)id);
	pthread_mutex_lock(&srv->users_mu);
	strmap_set(&srv->deleted_users, key, nullptr);
	pthread_mutex_unlock(&srv->users_mu);
}

struct server_config *server_config_get(struct server *srv) {
	pthread_mutex_lock(&srv->config_mu);
	struct server_config *c = srv->config;
	atomic_fetch_add(&c->refs, 1);
	pthread_mutex_unlock(&srv->config_mu);
	return c;
}

void server_config_set(struct server *srv, struct server_config *c) {
	pthread_mutex_lock(&srv->config_mu);
	struct server_config *old = srv->config;
	srv->config = c;
	pthread_mutex_unlock(&srv->config_mu);
	server_config_unref(old);
}

char *server_hostname(struct server *srv) {
	struct server_config *c = server_config_get(srv);
	char *h = xstrdup(c->cfg.basic.hostname);
	server_config_unref(c);
	return h;
}

void server_set_prefix(struct server *srv, struct irc_msg *msg) {
	AUTOFREE char *h = server_hostname(srv);
	irc_msg_set_prefix(msg, h, "", "");
}

// DB jobs complete on the DB thread's loop: their completions hop back to
// the loop which asked
struct hop {
	struct sloop *origin;
	db_job_fn run;
	db_done_fn done;
	server_cpu_done_fn cpu_done;
	cpu_work_fn work;
	struct server *srv;
	void *data;
	char *err;
};

static void hop_run(struct db *db, void *data) {
	struct hop *h = data;
	h->run(db, h->data);
}

static void hop_back(void *data) {
	struct hop *h = data;
	if (h->done != nullptr) {
		h->done(h->data);
	}
	if (h->cpu_done != nullptr) {
		h->cpu_done(h->data, h->err);
	}
	atomic_fetch_sub(&h->srv->pending_work, 1);
	xfree(h->err);
	xfree(h);
}

static void hop_drop(void *data) {
	// The origin loop is shutting down: run the completion anyway, it
	// frees the job's data
	hop_back(data);
}

static void hop_done(void *data) {
	struct hop *h = data;
	if (loop_current() == h->origin || h->origin == nullptr) {
		hop_back(h);
	} else {
		loop_post(h->origin, hop_back, hop_drop, h);
	}
}

void server_db(struct server *srv, db_job_fn run, db_done_fn done, void *data) {
	struct hop *h = xcalloc(1, sizeof(*h));
	*h = (struct hop){
		.origin = loop_current(), .run = run, .done = done, .data = data, .srv = srv};
	atomic_fetch_add(&srv->pending_work, 1);
	db_thread_submit(srv->db, hop_run, hop_done, h);
}

static void cpu_work_run(void *data) {
	struct hop *h = data;
	h->work(h->data);
}

static void cpu_submit(void *data) {
	struct hop *h = data;
	struct err *e = nullptr;
	if (!cpu_work_submit(h->srv->cpu, cpu_work_run, hop_done, h, &e)) {
		h->err = xstrdup(err_msg(e));
		err_free(e);
		hop_done(h);
	}
}

void server_cpu(struct server *srv, cpu_work_fn work, server_cpu_done_fn done, void *data) {
	struct hop *h = xcalloc(1, sizeof(*h));
	*h = (struct hop){
		.origin = loop_current(), .work = work, .cpu_done = done, .srv = srv, .data = data};
	atomic_fetch_add(&srv->pending_work, 1);
	struct sloop *zero = &srv->loops->loops[0];
	if (loop_current() == zero || loop_current() == nullptr) {
		cpu_submit(h);
	} else {
		loop_post(zero, cpu_submit, hop_drop, h);
	}
}

void server_config_reload(struct server *srv, struct server_config *c) {
	struct server_config *old = server_config_get(srv);
	c->cfg.basic.msg_store_driver = old->cfg.basic.msg_store_driver;
	xfree(c->cfg.basic.msg_store_source);
	c->cfg.basic.msg_store_source = xstrdup(old->cfg.basic.msg_store_source);
	server_config_unref(old);
	server_config_set(srv, c);
}

struct get_create_job {
	struct server *srv;
	char *username;
	bool enable, found;
	struct db_user record;
	struct err *err;
	server_user_done_fn done;
	void *data;
};
static void get_create_run(struct db *db, void *data) {
	struct get_create_job *j = data;
	if (!db_get_user(db, j->username, &j->record, &j->found, &j->err)) {
		return;
	}
	if (!j->found) {
		if (!j->enable) {
			bool failed = err_set(&j->err, 0, "user not found");
			(void)failed;
			return;
		}
		db_user_init(&j->record, j->username);
		j->found = true;
		bool ok = db_store_user(db, &j->record, &j->err);
		(void)ok;
	}
}
static void get_create_done(void *data) {
	struct get_create_job *j = data;
	if (j->err == nullptr && !atomic_load(&j->srv->shutting_down)) {
		struct db_user *copy = xmalloc(sizeof(*copy));
		db_user_copy(copy, &j->record);
		user_start_take(j->srv, copy);
	}
	j->done(j->data, j->found ? j->record.id : 0, j->err);
	if (j->found) {
		db_user_free(&j->record);
	}
	err_free(j->err);
	xfree(j->username);
	xfree(j);
}
void server_get_or_create_user(struct server *srv, const char *username, server_user_done_fn done,
			       void *data) {
	struct get_create_job *j = xcalloc(1, sizeof(*j));
	struct server_config *cfg = server_config_get(srv);
	*j = (struct get_create_job){.srv = srv,
				     .username = xstrdup(username),
				     .enable = cfg->cfg.basic.enable_users_on_auth,
				     .done = done,
				     .data = data};
	server_config_unref(cfg);
	server_db(srv, get_create_run, get_create_done, j);
}
