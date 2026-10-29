#include "user/internal.h"

#include <assert.h>

#include "base/alloc.h"
#include "irc/genmsg.h"
#include "net/url.h"

static void free_channel(void *data) {
	struct db_channel *ch = data;
	db_channel_free(ch);
	xfree(ch);
}

void user_network_start(struct user *u, struct user_network *net) {
	if (!net->record.enabled || !u->record.enabled || u->stopped ||
	    atomic_load(&u->srv->shutting_down)) {
		return;
	}
	struct upstream_events events = {.message = user_upstream_message,
					 .state = user_upstream_state,
					 .closed = user_upstream_closed,
					 .detach = user_upstream_detach,
					 .data = (void *)(uintptr_t)u->record.id};
	net->upstream = upstream_new(u->srv, &u->record, &net->record, &events);
	upstream_connect(net->upstream);
}

void user_network_free(struct user_network *net) {
	if (net == nullptr) {
		return;
	}
	upstream_free(net->upstream);
	db_network_free(&net->record);
	cmmap_free(&net->channels);
	cmmap_free(&net->latest_times);
	delivered_destroy(&net->delivered);
	xfree(net->last_error);
	xfree(net);
}

struct network_job {
	int64_t user_id;
	struct db_network record;
	bool remove;
	struct err *err;
};
static void network_job(struct db *db, void *data) {
	struct network_job *j = data;
	bool ok = j->remove ? db_delete_network(db, j->record.id, &j->err)
			    : db_store_network(db, j->user_id, &j->record, &j->err);
	(void)ok;
}

bool user_store_network(struct user *u, struct db_network *record, bool check_limit,
			struct err **err) {
	struct user_network *net = user_get_network(u, record->id);
	if (record->id != 0 && net == nullptr) {
		return err_set(err, 0, "unknown network");
	}
	if (record->addr[0] == '\0') {
		return err_set(err, 0, "network address cannot be empty");
	}
	const char *name = db_network_get_name(record);
	if (name[0] == '-' || cstr_eq(name, "*")) {
		return err_set(err, 0, "invalid network name");
	}
	for (size_t i = 0; i < u->networks.len; i++) {
		struct user_network *other = u->networks.data[i];
		if (other != net && cstr_eq(db_network_get_name(&other->record), name)) {
			return err_set(err, 0, "a network with the name %s already exists", name);
		}
	}
	struct url url;
	if (!network_url(record->addr, &url, err)) {
		return false;
	}
	bool valid = cstr_eq(url.scheme, "ircs") || cstr_eq(url.scheme, "irc+insecure") ||
		     cstr_eq(url.scheme, "irc+unix");
	url_free(&url);
	if (!valid) {
		return err_set(err, 0, "unknown URL scheme");
	}
	if (check_limit && record->enabled && (net == nullptr || !net->record.enabled)) {
		struct server_config *cfg = server_config_get(u->srv);
		int64_t limit = u->record.max_networks >= 0 ? u->record.max_networks
							    : cfg->cfg.basic.max_user_networks;
		server_config_unref(cfg);
		size_t enabled = 0;
		for (size_t i = 0; i < u->networks.len; i++) {
			assert(u->networks.data[i] != nullptr);
			enabled += u->networks.data[i]->record.enabled;
		}
		if (limit >= 0 && enabled >= (uint64_t)limit) {
			return err_set(err, 0, "maximum number of networks reached");
		}
	}
	struct network_job j = {.user_id = u->record.id};
	db_network_copy(&j.record, record);
	db_thread_call(u->srv->db, network_job, &j);
	if (j.err != nullptr) {
		db_network_free(&j.record);
		if (err != nullptr) {
			*err = j.err;
		} else {
			err_free(j.err);
		}
		return false;
	}
	record->id = j.record.id;
	if (net == nullptr) {
		net = xcalloc(1, sizeof(*net));
		cmmap_init(&net->channels, CASEMAPPING_ASCII, free_channel);
		cmmap_init(&net->latest_times, CASEMAPPING_ASCII, xfree);
		delivered_init(&net->delivered, CASEMAPPING_ASCII);
		net->last_error = xstrdup("");
		vec_push(&u->networks, net);
	} else {
		upstream_free(net->upstream);
		net->upstream = nullptr;
		net->joined_generation = 0;
		db_network_free(&net->record);
	}
	net->record = j.record;
	user_network_start(u, net);
	user_notify_network(u, net);
	return true;
}

static void network_attrs(struct user_network *net, struct irc_tags *attrs) {
	fill_network_addr_attrs(attrs, &net->record);
	irc_tags_set(attrs, "name", net->record.name);
	irc_tags_set(attrs, "nickname",
		     net->upstream != nullptr && net->upstream->attached ? net->upstream->nick
									 : net->record.nick);
	irc_tags_set(attrs, "state",
		     net->upstream != nullptr && net->upstream->attached ? "connected"
									 : "disconnected");
	irc_tags_set(attrs, "error", net->last_error);
}

static struct irc_msg *network_message(struct user_network *net) {
	struct irc_tags attrs = IRC_TAGS_INIT;
	network_attrs(net, &attrs);
	AUTO_STRBUF struct strbuf text = STRBUF_INIT;
	irc_tags_format(&attrs, &text);
	AUTOFREE char *id = xasprintf("%lld", (long long)net->record.id);
	struct irc_msg *msg = IRC_MSG("BOUNCER", "NETWORK", id, strbuf_cstr(&text));
	irc_tags_free(&attrs);
	return msg;
}

void user_notify_network(struct user *u, struct user_network *net) {
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *dc = u->clients.data[i];
		if (dc->parent_id == 0 &&
		    cap_registry_is_enabled(&dc->caps, "soju.im/bouncer-networks-notify")) {
			user_send_take(u, dc, network_message(net));
		}
	}
}

void user_send_networks(struct user *u, struct user_packet *p) {
	p->send_networks = true;
	for (size_t i = 0; i < u->networks.len; i++) {
		vec_push(&p->network_messages, network_message(u->networks.data[i]));
	}
}

bool user_delete_network(struct user *u, int64_t id, struct err **err) {
	struct user_network *net = user_get_network(u, id);
	if (net == nullptr) {
		return err_set(err, 0, "unknown network");
	}
	struct network_job j = {.user_id = u->record.id, .remove = true};
	db_network_copy(&j.record, &net->record);
	db_thread_call(u->srv->db, network_job, &j);
	db_network_free(&j.record);
	if (j.err != nullptr) {
		if (err != nullptr) {
			*err = j.err;
		} else {
			err_free(j.err);
		}
		return false;
	}
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *dc = u->clients.data[i];
		if (dc->network_id == id) {
			struct user_packet *p = user_packet_new(dc, nullptr);
			user_packet_add_take(p, IRC_MSG("ERROR", "Closing link"));
			p->close = true;
			user_packet_send_take(p);
		}
	}
	for (size_t i = 0; i < u->networks.len; i++) {
		if (u->networks.data[i] == net) {
			vec_remove(&u->networks, i);
			break;
		}
	}
	msgstore_delete_network(&u->store, id);
	user_network_free(net);
	AUTOFREE char *id_str = xasprintf("%lld", (long long)id);
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *dc = u->clients.data[i];
		if (dc->parent_id == 0 &&
		    cap_registry_is_enabled(&dc->caps, "soju.im/bouncer-networks-notify")) {
			user_send_take(u, dc, IRC_MSG("BOUNCER", "NETWORK", id_str, "*"));
		}
	}
	return true;
}

struct channel_job {
	int64_t network;
	struct db_channel record;
	struct err *err;
};
static void channel_job(struct db *db, void *data) {
	struct channel_job *j = data;
	bool ok = db_store_channel(db, j->network, &j->record, &j->err);
	(void)ok;
}
static void copy_channel(struct db_channel *out, const struct db_channel *in) {
	*out = *in;
	out->name = xstrdup(in->name);
	out->key = xstrdup(in->key);
	out->detached_internal_msg_id = xstrdup(in->detached_internal_msg_id);
}

bool user_store_channel(struct user *u, struct user_network *net, struct db_channel *record,
			struct err **err) {
	struct channel_job j = {.network = net->record.id};
	copy_channel(&j.record, record);
	db_thread_call(u->srv->db, channel_job, &j);
	if (j.err != nullptr) {
		db_channel_free(&j.record);
		if (err != nullptr) {
			*err = j.err;
		} else {
			err_free(j.err);
		}
		return false;
	}
	record->id = j.record.id;
	struct db_channel *saved = xmalloc(sizeof(*saved));
	*saved = j.record;
	cmmap_set(&net->channels, saved->name, saved);
	return true;
}

void user_detach_channel(struct user *u, struct user_network *net, const char *channel) {
	AUTOFREE char *channel_copy = xstrdup(channel);
	channel = channel_copy;
	struct db_channel *saved = cmmap_get(&net->channels, channel);
	if (saved == nullptr || saved->detached) {
		return;
	}
	struct db_channel record;
	copy_channel(&record, saved);
	record.detached = true;
	bool ok = user_store_channel(u, net, &record, nullptr);
	db_channel_free(&record);
	if (!ok) {
		return;
	}
	struct upstream *uc = net->upstream;
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *dc = u->clients.data[i];
		if (dc->network_id != net->record.id) {
			continue;
		}
		struct irc_msg *part = IRC_MSG("PART", channel, "Detach");
		irc_msg_set_prefix(part, uc != nullptr ? uc->nick : dc->nick,
				   uc != nullptr ? uc->username : dc->username,
				   uc != nullptr ? uc->hostname : dc->hostname);
		user_send_take(u, dc, part);
	}
}

void user_attach_channel(struct user *u, struct user_network *net, const char *channel) {
	AUTOFREE char *channel_copy = xstrdup(channel);
	channel = channel_copy;
	struct db_channel *saved = cmmap_get(&net->channels, channel);
	if (saved == nullptr || !saved->detached) {
		return;
	}
	struct db_channel record;
	copy_channel(&record, saved);
	record.detached = false;
	bool ok = user_store_channel(u, net, &record, nullptr);
	db_channel_free(&record);
	if (!ok || net->upstream == nullptr) {
		return;
	}
	struct upstream_channel *ch = cmmap_get(&net->upstream->channels, channel);
	if (ch == nullptr) {
		return;
	}
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *dc = u->clients.data[i];
		if (dc->network_id != net->record.id) {
			continue;
		}
		struct user_packet *p = user_packet_new(dc, nullptr);
		user_channel_replay(u, net, dc, p, ch);
		user_packet_send_take(p);
	}
}

struct receipts_job {
	int64_t network;
	char *client;
	db_receipt_vec receipts;
	struct err *err;
};
static void receipts_job(struct db *db, void *data) {
	struct receipts_job *j = data;
	bool ok = db_store_client_delivery_receipts(db, j->network, j->client, j->receipts.data,
						    j->receipts.len, &j->err);
	(void)ok;
}
void user_save_delivered(struct user *u, struct user_network *net, const char *client) {
	if (u->store.db == nullptr) {
		return;
	}
	struct receipts_job j = {.network = net->record.id, .client = xstrdup(client)};
	const char *target;
	void *v;
	for (size_t it = 0; cmmap_next(&net->delivered.targets, &it, &target, &v);) {
		const char *id = strmap_get(v, client);
		if (id == nullptr || id[0] == '\0') {
			continue;
		}
		struct db_delivery_receipt r;
		db_delivery_receipt_init(&r);
		xfree(r.target);
		xfree(r.client);
		xfree(r.internal_msg_id);
		r.target = xstrdup(target);
		r.client = xstrdup(client);
		r.internal_msg_id = xstrdup(id);
		vec_push(&j.receipts, r);
	}
	db_thread_call(u->srv->db, receipts_job, &j);
	if (j.err != nullptr) {
		log_info(&u->srv->log, "failed to save delivery receipts: %s", err_msg(j.err));
	}
	err_free(j.err);
	xfree(j.client);
	db_receipt_vec_free(&j.receipts);
}

void user_update_away_monitor(struct user *u, struct user_network *net) {
	struct upstream *uc = net->upstream;
	if (uc == nullptr || !uc->attached) {
		return;
	}
	bool away = true;
	struct strmap targets;
	strmap_init(&targets, nullptr);
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *dc = u->clients.data[i];
		if (dc->network_id != net->record.id) {
			continue;
		}
		if (dc->away == nullptr) {
			away = false;
		}
		const char *name;
		for (size_t it = 0; cmmap_next(&dc->monitored, &it, &name, nullptr);) {
			AUTOFREE char *cm = casemap_cstr(uc->casemap, name);
			if (!cstr_eq(cm, SERVICE_NICK_CM)) {
				strmap_set(&targets, cm, nullptr);
			}
		}
	}
	if (net->record.auto_away && away != uc->away) {
		uc->away = away;
		upstream_send_take(uc, away ? IRC_MSG("AWAY", "Auto away") : IRC_MSG("AWAY"));
	}
	if (strmap_has(&uc->isupport.m, "MONITOR")) {
		AUTOFREE char *want =
			casemap_cstr(uc->casemap, db_get_nick(&u->record, &net->record));
		AUTOFREE char *nick = casemap_cstr(uc->casemap, uc->nick);
		if (!cstr_eq(want, nick)) {
			strmap_set(&targets, want, nullptr);
		}
		strvec add = {0}, remove = {0};
		const char *name;
		for (size_t it = 0; strmap_next(&targets, &it, &name, nullptr);) {
			if (!cmmap_has(&uc->monitored, name)) {
				strvec_push(&add, name);
				bool *online = xcalloc(1, sizeof(*online));
				cmmap_set(&uc->monitored, name, online);
			}
		}
		for (size_t it = 0; cmmap_next(&uc->monitored, &it, &name, nullptr);) {
			if (!strmap_has(&targets, name)) {
				strvec_push(&remove, name);
			}
		}
		irc_msg_vec msgs = {0};
		generate_monitor(&msgs, "-", (const char *const *)remove.data, remove.len);
		generate_monitor(&msgs, "+", (const char *const *)add.data, add.len);
		for (size_t i = 0; i < msgs.len; i++) {
			upstream_send_take(uc, msgs.data[i]);
		}
		vec_free(&msgs);
		for (size_t i = 0; i < remove.len; i++) {
			cmmap_del(&uc->monitored, remove.data[i]);
		}
		strvec_free(&add);
		strvec_free(&remove);
	}
	strmap_free(&targets);
}
