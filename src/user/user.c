// User actors: lookups, the event queue, loading networks, start and
// stop, broadcasts (soju-rs user/mod.rs)
#include "user/internal.h"
#include <assert.h>
#include "base/alloc.h"

static thread_local struct strmap *users;

static char *user_key(struct server *srv, int64_t id) {
	return xasprintf("%p/%lld", (void *)srv, (long long)id);
}

struct user *user_lookup(struct server *srv, int64_t id) {
	if (users == nullptr) {
		return nullptr;
	}
	AUTOFREE char *key = user_key(srv, id);
	return strmap_get(users, key);
}

struct user_client *user_get_client(struct user *u, uint64_t id) {
	for (size_t i = 0; i < u->clients.len; i++) {
		if (u->clients.data[i]->id == id) {
			return u->clients.data[i];
		}
	}
	return nullptr;
}

struct user_network *user_get_network(struct user *u, int64_t id) {
	for (size_t i = 0; i < u->networks.len; i++) {
		if (u->networks.data[i]->record.id == id) {
			return u->networks.data[i];
		}
	}
	return nullptr;
}

struct user_network *user_find_network(struct user *u, const char *name) {
	for (size_t i = 0; i < u->networks.len; i++) {
		if (cstr_eq(db_network_get_name(&u->networks.data[i]->record), name) ||
		    cstr_eq(u->networks.data[i]->record.addr, name)) {
			return u->networks.data[i];
		}
	}
	return nullptr;
}

struct run_event {
	struct server *srv;
	int64_t id;
	user_run_fn run;
	loop_task_fn drop;
	void *data;
};

static void run_drop(void *data) {
	struct run_event *ev = data;
	if (ev->drop != nullptr) {
		ev->drop(ev->data);
	}
	atomic_fetch_sub(&ev->srv->pending_work, 1);
	xfree(ev);
}

static void run_event(void *data) {
	struct run_event *ev = data;
	struct user *u = user_lookup(ev->srv, ev->id);
	if (u != nullptr && !u->stopped) {
		ev->run(u, ev->data);
	} else if (ev->drop != nullptr) {
		ev->drop(ev->data);
	}
	atomic_fetch_sub(&ev->srv->pending_work, 1);
	xfree(ev);
}

void user_post(struct server *srv, int64_t id, user_run_fn run, loop_task_fn drop, void *data) {
	struct run_event *ev = xmalloc(sizeof(*ev));
	*ev = (struct run_event){.srv = srv, .id = id, .run = run, .drop = drop, .data = data};
	atomic_fetch_add(&srv->pending_work, 1);
	loop_post(loop_for_user(srv->loops, id), run_event, run_drop, ev);
}

void user_free_multiline(void *data) {
	struct multiline_batch *b = data;
	multiline_batch_free(b);
	xfree(b);
}

void user_client_free(struct user_client *dc) {
	if (dc == nullptr) {
		return;
	}
	xfree(dc->client);
	xfree(dc->nick);
	xfree(dc->username);
	xfree(dc->hostname);
	xfree(dc->realname);
	xfree(dc->away);
	cap_registry_free(&dc->caps);
	cmmap_free(&dc->monitored);
	strmap_free(&dc->multiline);
	strmap_free(&dc->metadata_subs);
	strbuf_free(&dc->peer_cert);
	xfree(dc);
}

struct loaded_network {
	struct db_network record;
	db_channel_vec channels;
	db_receipt_vec receipts;
	db_message_target_vec targets;
};
struct load_networks {
	int64_t user_id;
	VEC(struct loaded_network) networks;
	struct err *err;
};

static void load_networks(struct db *db, void *data) {
	struct load_networks *j = data;
	db_network_vec records = {0};
	if (!db_list_networks(db, j->user_id, &records, &j->err)) {
		return;
	}
	for (size_t i = 0; i < records.len; i++) {
		struct loaded_network n = {.record = records.data[i]};
		vec_push(&j->networks, n);
		struct loaded_network *out = &vec_last(&j->networks);
		if (!db_list_channels(db, n.record.id, &out->channels, &j->err) ||
		    !db_list_delivery_receipts(db, n.record.id, &out->receipts, &j->err) ||
		    !db_list_message_targets(db, n.record.id, &out->targets, &j->err)) {
			break;
		}
	}
	// The record strings moved into networks; release unprocessed records.
	for (size_t i = j->networks.len; i < records.len; i++) {
		db_network_free(&records.data[i]);
	}
	vec_free(&records);
}

static void free_channel(void *data) {
	struct db_channel *ch = data;
	db_channel_free(ch);
	xfree(ch);
}

static void free_loaded(struct load_networks *j) {
	for (size_t i = 0; i < j->networks.len; i++) {
		struct loaded_network *n = &j->networks.data[i];
		db_network_free(&n->record);
		db_channel_vec_free(&n->channels);
		db_receipt_vec_free(&n->receipts);
		db_message_target_vec_free(&n->targets);
	}
	vec_free(&j->networks);
	err_free(j->err);
}

// Retry delays of a user's loading, doubling from the first
static constexpr uint64_t LOAD_RETRY_MIN_MS = 1000;
static constexpr uint64_t LOAD_RETRY_MAX_MS = 60000;

// Owns its timer: the user may be freed before it's closed
struct load_retry {
	uv_timer_t timer;
	struct server *srv;
	int64_t user_id;
	uint64_t delay_ms;
};

static void load_retry_closed(uv_handle_t *h) {
	struct load_retry *r = h->data;
	atomic_fetch_sub(&r->srv->pending_work, 1);
	xfree(r);
}

static void load_user(struct user *u);

static void load_retry_fired(uv_timer_t *t) {
	struct load_retry *r = t->data;
	struct user *u = user_lookup(r->srv, r->user_id);
	if (u != nullptr && u->load_retry == r && !u->stopped) {
		load_user(u);
	}
}

void user_load_retry_stop(struct user *u) {
	if (u->load_retry != nullptr) {
		uv_timer_stop(&u->load_retry->timer);
		uv_close((uv_handle_t *)&u->load_retry->timer, load_retry_closed);
		u->load_retry = nullptr;
	}
}

// Loads the user's networks with their channels, receipts and flagged
// targets as a whole. A transient database error (e.g. a Turso timeout)
// must not leave the user without one of their networks until restart:
// the load is retried until it succeeds or the user stops.
static void load_user(struct user *u) {
	struct server *srv = u->srv;
	struct load_networks j = {.user_id = u->record.id};
	db_thread_call(srv->db, load_networks, &j);
	if (j.err != nullptr) {
		struct load_retry *r = u->load_retry;
		if (r == nullptr) {
			r = xcalloc(1, sizeof(*r));
			*r = (struct load_retry){
				.srv = srv, .user_id = u->record.id, .delay_ms = LOAD_RETRY_MIN_MS};
			atomic_fetch_add(&srv->pending_work, 1);
			uv_timer_init(&u->loop->uv, &r->timer);
			r->timer.data = r;
			u->load_retry = r;
		} else {
			r->delay_ms = r->delay_ms * 2 < LOAD_RETRY_MAX_MS ? r->delay_ms * 2
									  : LOAD_RETRY_MAX_MS;
		}
		log_info(&srv->log, "user \"%s\": failed to load, retrying in %llu ms: %s",
			 u->record.username, (unsigned long long)r->delay_ms, err_msg(j.err));
		free_loaded(&j);
		uv_timer_start(&r->timer, load_retry_fired, r->delay_ms, 0);
		return;
	}
	user_load_retry_stop(u);
	for (size_t i = 0; i < j.networks.len; i++) {
		struct loaded_network *loaded = &j.networks.data[i];
		struct user_network *net = xcalloc(1, sizeof(*net));
		net->record = loaded->record;
		net->last_error = xstrdup("");
		cmmap_init(&net->channels, CASEMAPPING_ASCII, free_channel);
		cmmap_init(&net->latest_times, CASEMAPPING_ASCII, xfree);
		cmmap_init(&net->push_targets, CASEMAPPING_ASCII, xfree);
		cmmap_init(&net->blocked_targets, CASEMAPPING_RFC1459, nullptr);
		for (size_t k = 0; k < loaded->targets.len; k++) {
			if (loaded->targets.data[k].blocked) {
				cmmap_set(&net->blocked_targets, loaded->targets.data[k].target,
					  nullptr);
			}
		}
		db_message_target_vec_free(&loaded->targets);
		delivered_init(&net->delivered, CASEMAPPING_ASCII);
		for (size_t k = 0; k < loaded->channels.len; k++) {
			struct db_channel *ch = xmalloc(sizeof(*ch));
			*ch = loaded->channels.data[k];
			cmmap_set(&net->channels, ch->name, ch);
		}
		vec_free(&loaded->channels);
		for (size_t k = 0; k < loaded->receipts.len; k++) {
			struct db_delivery_receipt *r = &loaded->receipts.data[k];
			delivered_set(&net->delivered, r->target, r->client, r->internal_msg_id);
		}
		db_receipt_vec_free(&loaded->receipts);
		vec_push(&u->networks, net);
		if (u->record.enabled) {
			user_network_start(u, net);
		}
	}
	vec_free(&j.networks);
	// Clients connected while loading learn about the networks
	for (size_t i = 0; i < u->networks.len && u->clients.len > 0; i++) {
		user_notify_network(u, u->networks.data[i]);
	}
}

struct start_event {
	struct server *srv;
	struct db_user *record;
};
static void start_drop(void *data) {
	struct start_event *ev = data;
	db_user_free(ev->record);
	xfree(ev->record);
	atomic_fetch_sub(&ev->srv->pending_work, 1);
	xfree(ev);
}

static void start_run(void *data) {
	struct start_event *ev = data;
	struct server *srv = ev->srv;
	struct db_user *record = ev->record;
	if (atomic_load(&srv->shutting_down) || user_lookup(srv, record->id) != nullptr ||
	    server_user_deleted(srv, record->id)) {
		start_drop(ev);
		return;
	}
	struct user *u = xcalloc(1, sizeof(*u));
	REF_TRACK(&u->ref, "user");
	user_web_push_init(u);
	u->srv = srv;
	u->loop = loop_current();
	u->record = *record;
	xfree(record);
	xfree(ev);
	struct server_config *cfg = server_config_get(srv);
	msgstore_init(&u->store,
		      cfg->cfg.basic.msg_store_driver == MSG_STORE_MEMORY ? nullptr : srv->db);
	// Turso: lines are buffered and written in batches
	msgstore_start_batches(&u->store, &u->loop->uv, &srv->log, u->record.id,
			       cfg->cfg.basic.chat_history_limit);
	server_config_unref(cfg);
	if (users == nullptr) {
		users = xcalloc(1, sizeof(*users));
		strmap_init(users, nullptr);
	}
	AUTOFREE char *key = user_key(srv, u->record.id);
	strmap_set(users, key, u);
	pthread_mutex_lock(&srv->users_mu);
	int64_t *id = xmalloc(sizeof(*id));
	*id = u->record.id;
	strmap_set(&srv->users, u->record.username, id);
	pthread_mutex_unlock(&srv->users_mu);
	load_user(u);
	atomic_fetch_sub(&srv->pending_work, 1);
}

void user_start_take(struct server *srv, struct db_user *record) {
	struct start_event *ev = xmalloc(sizeof(*ev));
	*ev = (struct start_event){.srv = srv, .record = record};
	atomic_fetch_add(&srv->pending_work, 1);
	loop_post(loop_for_user(srv->loops, record->id), start_run, start_drop, ev);
}

struct load_users {
	db_user_vec users;
	struct err *err;
};
static void load_users(struct db *db, void *data) {
	struct load_users *j = data;
	bool ok = db_list_users(db, &j->users, &j->err);
	(void)ok;
}

bool user_server_start(struct server *srv, struct err **err) {
	struct load_users j = {0};
	db_thread_call(srv->db, load_users, &j);
	if (j.err != nullptr) {
		if (err != nullptr) {
			*err = j.err;
		} else {
			err_free(j.err);
		}
		db_user_vec_free(&j.users);
		return false;
	}
	for (size_t i = 0; i < j.users.len; i++) {
		struct db_user *record = xmalloc(sizeof(*record));
		*record = j.users.data[i];
		user_start_take(srv, record);
	}
	vec_free(&j.users);
	return true;
}

void user_close_clients(struct user *u, const char *reason) {
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_packet *p = user_packet_new(u->clients.data[i], nullptr);
		user_packet_add_take(p, IRC_MSG("ERROR", reason));
		p->close = true;
		user_packet_send_take(p);
	}
}

static void stop_user(struct user *u, void *data) {
	(void)data;
	u->stopped = true;
	user_close_clients(u, "Server shutting down");
	for (size_t i = 0; i < u->networks.len; i++) {
		struct user_network *net = u->networks.data[i];
		struct strmap clients;
		strmap_init(&clients, nullptr);
		void *v;
		for (size_t it = 0; cmmap_next(&net->delivered.targets, &it, nullptr, &v);) {
			const char *name;
			for (size_t jt = 0; strmap_next(v, &jt, &name, nullptr);) {
				strmap_set(&clients, name, nullptr);
			}
		}
		const char *name;
		for (size_t it = 0; strmap_next(&clients, &it, &name, nullptr);) {
			user_save_delivered(u, net, name);
		}
		strmap_free(&clients);
		user_network_free(net);
	}
	vec_free(&u->networks);
	for (size_t i = 0; i < u->clients.len; i++) {
		user_client_free(u->clients.data[i]);
	}
	vec_free(&u->clients);
	user_web_push_stop(u);
	user_mentions_stop(u);
	user_load_retry_stop(u);
	msgstore_destroy(&u->store);
	pthread_mutex_lock(&u->srv->users_mu);
	strmap_remove(&u->srv->users, u->record.username);
	pthread_mutex_unlock(&u->srv->users_mu);
	AUTOFREE char *key = user_key(u->srv, u->record.id);
	(void)strmap_take(users, key);
	if (strmap_len(users) == 0) {
		strmap_free(users);
		xfree(users);
		users = nullptr;
	}
	db_user_free(&u->record);
	ref_untrack(&u->ref);
	xfree(u);
}

void user_stop(struct user *u) {
	server_mark_user_deleted(u->srv, u->record.id);
	stop_user(u, nullptr);
}

struct stop_loop {
	struct server *srv;
};
static void stop_loop_drop(void *data) {
	struct stop_loop *ev = data;
	atomic_fetch_sub(&ev->srv->pending_work, 1);
	xfree(ev);
}

static void stop_loop(void *data) {
	struct stop_loop *ev = data;
	VEC(int64_t) ids = {0};
	if (users != nullptr) {
		void *v;
		for (size_t it = 0; strmap_next(users, &it, nullptr, &v);) {
			struct user *u = v;
			if (u->srv == ev->srv) {
				vec_push(&ids, u->record.id);
			}
		}
	}
	for (size_t i = 0; i < ids.len; i++) {
		struct user *u = user_lookup(ev->srv, ids.data[i]);
		if (u != nullptr) {
			stop_user(u, nullptr);
		}
	}
	vec_free(&ids);
	downstream_close_server(ev->srv);
	atomic_fetch_sub(&ev->srv->pending_work, 1);
	xfree(ev);
}

void user_server_stop(struct server *srv) {
	for (unsigned i = 0; i < srv->loops->n; i++) {
		struct stop_loop *ev = xmalloc(sizeof(*ev));
		ev->srv = srv;
		atomic_fetch_add(&srv->pending_work, 1);
		loop_post(&srv->loops->loops[i], stop_loop, stop_loop_drop, ev);
	}
}

static void broadcast_user(struct user *u, void *data) {
	struct irc_msg *msg = data;
	for (size_t i = 0; i < u->clients.len; i++) {
		user_send_take(u, u->clients.data[i], irc_msg_clone(msg));
	}
	irc_msg_free(msg);
}
static void drop_message(void *data) {
	irc_msg_free(data);
}
void user_broadcast_take(struct server *srv, struct irc_msg *msg) {
	pthread_mutex_lock(&srv->users_mu);
	void *v;
	for (size_t it = 0; strmap_next(&srv->users, &it, nullptr, &v);) {
		user_post(srv, *(int64_t *)v, broadcast_user, drop_message, irc_msg_clone(msg));
	}
	pthread_mutex_unlock(&srv->users_mu);
	irc_msg_free(msg);
}
