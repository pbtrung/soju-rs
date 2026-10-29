#include "server/server.h"

#include "base/alloc.h"

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
	xfree(c);
}

void server_init(struct server *srv, struct loop_pool *loops, struct db_thread *db) {
	*srv = (struct server){.loops = loops, .db = db};
	log_init(&srv->log, &srv->log_root);
	pthread_mutex_init(&srv->config_mu, nullptr);
	srv->config = server_config_default();
	srv->cpu = cpu_work_pool_new(&loops->loops[0].uv, 0);
	atomic_init(&srv->next_downstream_id, 1);
}

void server_finish(struct server *srv) {
	cpu_work_pool_free(srv->cpu);
	server_config_unref(srv->config);
	pthread_mutex_destroy(&srv->config_mu);
	log_free(&srv->log);
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
	*h = (struct hop){.origin = loop_current(), .run = run, .done = done, .data = data};
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
	struct sloop *zero = &srv->loops->loops[0];
	if (loop_current() == zero || loop_current() == nullptr) {
		cpu_submit(h);
	} else {
		loop_post(zero, cpu_submit, hop_drop, h);
	}
}
