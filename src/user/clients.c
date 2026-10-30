// The downstream connections of users: client snapshots, attaching to
// networks, registration, input and disconnection (soju-rs user/mod.rs)
#include "user/internal.h"
#include <assert.h>
#include "base/alloc.h"

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
	strmap_init(&c->multiline, user_free_multiline);
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
				user_channel_replay(u, net, dc, p, ch, true);
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
