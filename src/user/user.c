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

static void free_multiline(void *data) {
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

static struct user_client *client_snapshot(struct downstream *dc) {
	struct user_client *c = xcalloc(1, sizeof(*c));
	*c = (struct user_client){.srv = dc->srv,
				  .queued = conn_queued(dc->conn),
				  .id = dc->id,
				  .parent_id = dc->has_parent ? dc->parent_id : 0,
				  .network_id = dc->has_network      ? dc->network_id
						: dc->reg != nullptr ? dc->reg->network_id
								     : 0,
				  .loop = loop_current(),
				  .client = xstrdup(dc->client_name),
				  .nick = xstrdup(dc->nick),
				  .username = xstrdup(dc->username),
				  .hostname = xstrdup(dc->hostname),
				  .realname = xstrdup(dc->realname),
				  .away = dc->away != nullptr ? xstrdup(dc->away) : nullptr,
				  .impersonating = dc->impersonating,
				  .has_remote_ip = dc->has_remote_ip,
				  .remote_ip = dc->remote_ip};
	c->srv = dc->srv;
	c->queued = conn_queued(dc->conn);
	cap_registry_init(&c->caps);
	const char *key;
	void *v;
	for (size_t it = 0; strmap_next(&dc->caps.available, &it, &key, &v);) {
		cap_registry_set_available(&c->caps, key, v);
	}
	for (size_t it = 0; strmap_next(&dc->caps.enabled, &it, &key, nullptr);) {
		cap_registry_set_enabled(&c->caps, key, true);
	}
	cmmap_init(&c->monitored, dc->casemap, nullptr);
	strmap_init(&c->multiline, free_multiline);
	strmap_init(&c->metadata_subs, nullptr);
	for (size_t it = 0; strmap_next(&dc->metadata_subs, &it, &key, nullptr);) {
		strmap_set(&c->metadata_subs, key, nullptr);
	}
	strbuf_append_str(&c->peer_cert, strbuf_str(&dc->peer_cert));
	return c;
}

static void update_client(struct user *u, void *data) {
	struct user_client *fresh = data, *dc = user_get_client(u, fresh->id);
	if (dc != nullptr) {
		dc->queued = fresh->queued;
		xfree(dc->nick);
		dc->nick = xstrdup(fresh->nick);
		xfree(dc->username);
		dc->username = xstrdup(fresh->username);
		xfree(dc->hostname);
		dc->hostname = xstrdup(fresh->hostname);
		xfree(dc->realname);
		dc->realname = xstrdup(fresh->realname);
		cap_registry_free(&dc->caps);
		dc->caps = fresh->caps;
		cap_registry_init(&fresh->caps);
		// The user actor owns MONITOR, AWAY and multiline assembly state.
	}
	user_client_free(fresh);
}

static void drop_client(void *data) {
	user_client_free(data);
}

void user_downstream_updated(struct downstream *dc, void *data) {
	(void)data;
	if (dc->user_id != 0 && !dc->closed) {
		user_post(dc->srv, dc->user_id, update_client, drop_client, client_snapshot(dc));
	}
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
	struct load_networks j = {.user_id = u->record.id};
	db_thread_call(srv->db, load_networks, &j);
	if (j.err != nullptr) {
		log_info(&srv->log, "failed to load user networks: %s", err_msg(j.err));
	}
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
	err_free(j.err);
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

struct attach_event {
	struct user_client *client;
	char *network_name;
	bool server_checked;
};
static void attach_drop(void *data) {
	struct attach_event *ev = data;
	user_client_free(ev->client);
	xfree(ev->network_name);
	xfree(ev);
}

static void attach_cancel(void *data) {
	struct attach_event *ev = data;
	if (ev->client != nullptr) {
		struct user_packet *p = user_packet_new(ev->client, nullptr);
		user_packet_add_take(p, IRC_MSG("ERROR", "User stopped"));
		p->close = true;
		user_packet_send_take(p);
	}
	attach_drop(ev);
}

static void attach_run(struct user *u, void *data) {
	struct attach_event *ev = data;
	struct user_client *dc = ev->client;
	struct user_packet *p = user_packet_new(dc, nullptr);
	struct user_network *net = nullptr;
	if (!u->record.enabled || atomic_load(&u->srv->shutting_down)) {
		user_packet_add_take(p, IRC_MSG("ERROR", "This bouncer account is disabled"));
		p->close = true;
		goto out;
	}
	if (dc->network_id != 0) {
		net = user_get_network(u, dc->network_id);
	} else if (ev->network_name[0] != '\0') {
		net = user_find_network(u, ev->network_name);
	}
	if (net == nullptr && ev->network_name[0] != '\0' && !cstr_eq(ev->network_name, "*") &&
	    ev->server_checked) {
		struct db_network n;
		db_network_init(&n, ev->network_name);
		bool stored = user_store_network(u, &n, true, nullptr);
		if (stored) {
			net = user_get_network(u, n.id);
		}
		db_network_free(&n);
	}
	if ((dc->network_id != 0 || ev->network_name[0] != '\0') && net == nullptr) {
		AUTOFREE char *error = xasprintf("Failed to connect to \"%s\"", ev->network_name);
		user_packet_add_take(p, IRC_MSG("464", "*", error));
		p->close = true;
		goto out;
	}
	dc->network_id = net != nullptr ? net->record.id : 0;
	vec_push(&u->clients, dc);
	ev->client = nullptr;
	user_packet_sync(u, net, p, true);
	if (cap_registry_is_enabled(&dc->caps, "soju.im/bouncer-networks-notify") &&
	    dc->parent_id == 0) {
		user_send_networks(u, p);
	}
	if (net != nullptr && net->upstream != nullptr && net->upstream->attached) {
		const char *name;
		void *v;
		for (size_t it = 0; cmmap_next(&net->upstream->channels, &it, &name, &v);) {
			struct upstream_channel *ch = v;
			struct db_channel *saved = cmmap_get(&net->channels, name);
			if (ch->complete && (saved == nullptr || !saved->detached)) {
				user_channel_replay(u, net, dc, p, ch);
			}
		}
	}
out:
	if (p->welcome && cap_registry_is_enabled(&dc->caps, "draft/metadata-2")) {
		user_initial_metadata(u, dc, p);
	} else {
		user_packet_send_take(p);
	}
	if (ev->client == nullptr && net != nullptr &&
	    !cap_registry_is_enabled(&dc->caps, "draft/chathistory")) {
		bool first = true;
		for (size_t i = 0; i < u->clients.len; i++) {
			struct user_client *other = u->clients.data[i];
			if (other != dc && other->network_id == net->record.id &&
			    cstr_eq(other->client, dc->client)) {
				first = false;
			}
		}
		if (first) {
			const char *target;
			for (size_t it = 0;
			     cmmap_next(&net->delivered.targets, &it, &target, nullptr);) {
				const char *last =
					delivered_get(&net->delivered, target, dc->client);
				if (last[0] == '\0') {
					continue;
				}
				struct user_packet *history = user_packet_new(dc, nullptr);
				history->batch_type = xstrdup("chathistory");
				strvec_push(&history->batch_params, target);
				AUTOFREE char *canonical = casemap_cstr(
					net->upstream != nullptr ? net->upstream->casemap
								 : CASEMAPPING_RFC1459,
					target);
				bool loaded = msgstore_load_latest_id(
					&u->store, net->record.id, canonical, last, BACKLOG_LIMIT,
					false, &history->messages, nullptr);
				(void)loaded;
				user_packet_send_take(history);
				char *latest = nullptr;
				if (msgstore_last_id(&u->store, net->record.id, canonical, &latest,
						     nullptr)) {
					delivered_set(&net->delivered, target, dc->client, latest);
				}
				xfree(latest);
			}
		}
	}
	attach_drop(ev);
	if (net != nullptr) {
		user_update_away_monitor(u, net);
	}
}

static void attach_downstream(struct downstream *dc) {
	struct attach_event *ev = xcalloc(1, sizeof(*ev));
	ev->client = client_snapshot(dc);
	ev->network_name = xstrdup(dc->reg != nullptr ? dc->reg->network_name : "");
	ev->server_checked = dc->reg != nullptr && dc->reg->server_checked;
	user_post(dc->srv, dc->user_id, attach_run, attach_cancel, ev);
}

struct register_job {
	struct server *srv;
	uint64_t dc_id;
	char *username;
	struct db_user record;
	bool found;
	struct err *err;
};
static void register_load(struct db *db, void *data) {
	struct register_job *j = data;
	if (!db_get_user(db, j->username, &j->record, &j->found, &j->err)) {
		return;
	}
	if (j->found && !j->record.enabled) {
		struct server_config *cfg = server_config_get(j->srv);
		bool enable = cfg->cfg.basic.enable_users_on_auth;
		server_config_unref(cfg);
		if (enable) {
			j->record.enabled = true;
			bool ok = db_store_user_keep_password(db, &j->record, &j->err);
			(void)ok;
		}
	}
}
static void register_done(void *data) {
	struct register_job *j = data;
	struct downstream *dc = downstream_lookup(j->dc_id);
	if (dc != nullptr) {
		if (j->err != nullptr || !j->found || server_user_deleted(j->srv, j->record.id) ||
		    atomic_load(&j->srv->shutting_down)) {
			downstream_send(dc, IRC_MSG("ERROR", "Internal server error"));
			downstream_close(dc);
		} else {
			dc->user_id = j->record.id;
			dc->user_loop = loop_for_user(j->srv->loops, j->record.id);
			struct db_user *record = xmalloc(sizeof(*record));
			db_user_copy(record, &j->record);
			user_start_take(j->srv, record);
			attach_downstream(dc);
		}
	}
	if (j->found) {
		db_user_free(&j->record);
	}
	err_free(j->err);
	xfree(j->username);
	xfree(j);
}

void user_downstream_registered(struct downstream *dc, void *data) {
	(void)data;
	if (dc->has_parent && dc->user_id != 0) {
		attach_downstream(dc);
		return;
	}
	struct register_job *j = xcalloc(1, sizeof(*j));
	dc->busy = true;
	*j = (struct register_job){
		.srv = dc->srv, .dc_id = dc->id, .username = xstrdup(dc->username)};
	server_db(dc->srv, register_load, register_done, j);
}

struct client_input {
	struct user_client *snapshot;
	struct irc_msg *msg;
	uint64_t ack_id;
};
static void client_input_drop(void *data) {
	struct client_input *ev = data;
	struct user_packet *p = user_packet_new(ev->snapshot, nullptr);
	p->ack = true;
	p->ack_id = ev->ack_id;
	user_packet_send_take(p);
	user_client_free(ev->snapshot);
	irc_msg_free(ev->msg);
	xfree(ev);
}
static void client_input(struct user *u, void *data) {
	struct client_input *ev = data;
	uint64_t id = ev->snapshot->id;
	update_client(u, ev->snapshot);
	ev->snapshot = nullptr;
	struct user_client *dc = user_get_client(u, id);
	if (dc != nullptr) {
		struct irc_msg *msg = ev->msg;
		ev->msg = nullptr;
		user_handle_command(u, dc, msg, ev->ack_id);
	} else {
		// The source may be a live parent whose child was detached.
		struct user_client *parent = user_get_client(u, ev->ack_id);
		if (parent != nullptr) {
			struct user_packet *p = user_packet_new(parent, nullptr);
			p->ack = true;
			p->ack_id = ev->ack_id;
			user_packet_send_take(p);
		}
	}
	irc_msg_free(ev->msg);
	xfree(ev);
}

void user_downstream_message(struct downstream *dc, struct irc_msg *msg, void *data) {
	(void)data;
	struct client_input *ev = xcalloc(1, sizeof(*ev));
	*ev = (struct client_input){.snapshot = client_snapshot(dc),
				    .msg = msg,
				    .ack_id = dc->has_parent ? dc->parent_id : dc->id};
	user_post(dc->srv, dc->user_id, client_input, client_input_drop, ev);
}

struct disconnected {
	uint64_t id;
};
static void disconnected(struct user *u, void *data) {
	struct disconnected *ev = data;
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *dc = u->clients.data[i];
		if (dc->id != ev->id) {
			continue;
		}
		struct user_network *net = user_get_network(u, dc->network_id);
		if (net != nullptr) {
			user_save_delivered(u, net, dc->client);
		}
		for (size_t j = 0; j < u->networks.len; j++) {
			if (u->networks.data[j]->upstream != nullptr) {
				upstream_cancel_downstream(u->networks.data[j]->upstream, dc->id);
			}
		}
		user_client_free(dc);
		vec_remove(&u->clients, i);
		if (net != nullptr) {
			user_update_away_monitor(u, net);
		}
		break;
	}
	xfree(ev);
}
void user_downstream_closed(struct downstream *dc, void *data) {
	(void)data;
	if (dc->user_id == 0) {
		return;
	}
	struct disconnected *ev = xmalloc(sizeof(*ev));
	ev->id = dc->id;
	user_post(dc->srv, dc->user_id, disconnected, xfree, ev);
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
