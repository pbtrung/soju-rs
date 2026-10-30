#include "user/internal.h"

#include <assert.h>

#include "base/alloc.h"
#include "base/utf8.h"
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
					 .channel_complete = user_upstream_channel_complete,
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
	cmmap_free(&net->push_targets);
	cmmap_free(&net->blocked_targets);
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

// soju-rs User::check_network
static bool check_network(const struct user *u, const struct db_network *record,
			  const struct user_network *net, struct err **err) {
	struct url url;
	if (!network_url(record->addr, &url, err)) {
		return false;
	}
	bool ok = true;
	bool tcp = cstr_eq(url.scheme, "ircs") || cstr_eq(url.scheme, "irc+insecure");
	bool unix = cstr_eq(url.scheme, "irc+unix") || cstr_eq(url.scheme, "unix");
	if (url.has_user) {
		ok = err_set(err, 0, "%s:// URL must not have username and password information",
			     url.scheme);
	} else if (url.query[0] != '\0') {
		ok = err_set(err, 0, "%s:// URL must not have query values", url.scheme);
	} else if (url.fragment[0] != '\0') {
		ok = err_set(err, 0, "%s:// URL must not have a fragment", url.scheme);
	} else if (tcp && url.host[0] == '\0') {
		ok = err_set(err, 0, "%s:// URL must have a host", url.scheme);
	} else if (tcp && url.path[0] != '\0') {
		ok = err_set(err, 0, "%s:// URL must not have a path", url.scheme);
	} else if (unix && !u->record.admin) {
		ok = err_set(err, 0, "non-admin users cannot add networks with a unix address");
	} else if (unix && url.host[0] != '\0') {
		ok = err_set(err, 0, "%s:// URL must not have a host", url.scheme);
	} else if (unix && url.path[0] == '\0') {
		ok = err_set(err, 0, "%s:// URL must have a path", url.scheme);
	} else if (!tcp && !unix) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(url.scheme));
		ok = err_set(err, 0, "unknown URL scheme %s", q);
	}
	url_free(&url);
	if (!ok) {
		return false;
	}
	const char *name = db_network_get_name(record);
	if (name[0] == '\0') {
		return err_set(err, 0, "network name cannot be empty");
	}
	if (name[0] == '-') {
		// Can be mixed up with flags when sending commands to the service
		return err_set(err, 0, "network name cannot start with a dash character");
	}
	for (size_t i = 0; i < u->networks.len; i++) {
		struct user_network *other = u->networks.data[i];
		if (other != net && cstr_eq(db_network_get_name(&other->record), name)) {
			AUTOFREE char *q = utf8_debug_cstr(str_from(name));
			return err_set(err, 0, "a network with the name %s already exists", q);
		}
	}
	return true;
}

bool user_store_network(struct user *u, struct db_network *record, bool check_limit,
			struct err **err) {
	struct user_network *net = user_get_network(u, record->id);
	if (record->id != 0 && net == nullptr) {
		return err_set(err, 0, "unknown network");
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
			return err_set(err, 0, "maximum number of enabled networks reached");
		}
	}
	if (!check_network(u, record, net, err)) {
		return false;
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
		cmmap_init(&net->push_targets, CASEMAPPING_ASCII, xfree);
		cmmap_init(&net->blocked_targets, CASEMAPPING_RFC1459, nullptr);
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

bool user_save_network(struct user *u, struct user_network *net, const struct db_network *record,
		       struct err **err) {
	struct network_job j = {.user_id = u->record.id};
	db_network_copy(&j.record, record);
	db_thread_call(u->srv->db, network_job, &j);
	if (j.err != nullptr) {
		db_network_free(&j.record);
		*err = j.err;
		return false;
	}
	db_network_free(&net->record);
	net->record = j.record;
	if (net->upstream != nullptr) {
		db_network_free(&net->upstream->network);
		db_network_copy(&net->upstream->network, &net->record);
	}
	return true;
}

struct user_job {
	struct db_user record;
	struct err *err;
};
static void user_job(struct db *db, void *data) {
	struct user_job *j = data;
	bool ok = db_store_user_keep_password(db, &j->record, &j->err);
	(void)ok;
}
bool user_save_record(struct user *u, const struct db_user *record, struct err **err) {
	struct user_job j = {0};
	db_user_copy(&j.record, record);
	db_thread_call(u->srv->db, user_job, &j);
	if (j.err != nullptr) {
		db_user_free(&j.record);
		*err = j.err;
		return false;
	}
	bool nick_changed = !cstr_eq(u->record.nick, j.record.nick);
	bool realname_changed = !cstr_eq(u->record.realname, j.record.realname);
	db_user_free(&u->record);
	u->record = j.record;
	// soju-rs update_user: the networks using the user's nick and realname
	// get the new ones
	for (size_t i = 0; i < u->networks.len; i++) {
		struct user_network *net = u->networks.data[i];
		struct upstream *uc = net->upstream;
		if (uc != nullptr) {
			db_user_free(&uc->user);
			db_user_copy(&uc->user, &u->record);
		}
		if (uc != nullptr && nick_changed && net->record.nick[0] == '\0') {
			upstream_send_take(uc,
					   IRC_MSG("NICK", db_get_nick(&u->record, &net->record)));
		}
		if (!realname_changed || net->record.realname[0] != '\0') {
			continue;
		}
		if (uc != nullptr && cap_registry_is_enabled(&uc->caps, "setname")) {
			upstream_send_take(
				uc, IRC_MSG("SETNAME", db_get_realname(&u->record, &net->record)));
		} else if (uc != nullptr) {
			// Reconnects with the new realname
			upstream_free(uc);
			net->upstream = nullptr;
			net->joined_generation = 0;
			user_notify_network(u, net);
			user_network_start(u, net);
		}
	}
	return true;
}

static void network_attrs(struct user *u, struct user_network *net, struct irc_tags *attrs) {
	fill_network_addr_attrs(attrs, &net->record);
	irc_tags_set(attrs, "name", net->record.name);
	irc_tags_set(attrs, "nickname", db_get_nick(&u->record, &net->record));
	if (net->record.username[0]) {
		irc_tags_set(attrs, "username", net->record.username);
	}
	const char *realname = db_get_realname(&u->record, &net->record);
	if (realname[0]) {
		irc_tags_set(attrs, "realname", realname);
	}
	irc_tags_set(attrs, "state",
		     net->upstream != nullptr && net->upstream->attached ? "connected"
									 : "disconnected");
	if (net->last_error[0]) {
		irc_tags_set(attrs, "error", net->last_error);
	}
}

static struct irc_msg *network_message(struct user *u, struct user_network *net) {
	struct irc_tags attrs = IRC_TAGS_INIT;
	network_attrs(u, net, &attrs);
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
			user_send_take(u, dc, network_message(u, net));
		}
	}
}

void user_notify_network_attrs(struct user *u, struct user_network *net, const char *attrs) {
	AUTOFREE char *id = xasprintf("%lld", (long long)net->record.id);
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *dc = u->clients.data[i];
		if (dc->parent_id == 0 &&
		    cap_registry_is_enabled(&dc->caps, "soju.im/bouncer-networks-notify")) {
			user_send_take(u, dc, IRC_MSG("BOUNCER", "NETWORK", id, attrs));
		}
	}
}

void user_send_networks(struct user *u, struct user_packet *p) {
	p->send_networks = true;
	for (size_t i = 0; i < u->networks.len; i++) {
		vec_push(&p->network_messages, network_message(u, u->networks.data[i]));
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

struct delete_channel_job {
	int64_t id;
	struct err *err;
};
static void delete_channel_job(struct db *db, void *data) {
	struct delete_channel_job *j = data;
	bool ok = db_delete_channel(db, j->id, &j->err);
	(void)ok;
}
bool user_delete_channel(struct user *u, struct user_network *net, const char *channel,
			 struct err **err) {
	struct db_channel *saved = cmmap_get(&net->channels, channel);
	if (saved == nullptr) {
		return true;
	}
	struct delete_channel_job j = {.id = saved->id};
	db_thread_call(u->srv->db, delete_channel_job, &j);
	if (j.err != nullptr) {
		*err = j.err;
		return false;
	}
	cmmap_del(&net->channels, channel);
	return true;
}

// soju-rs User::detach_channel: the backlog of the channel starts after
// the last message, for its reattachment
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
	struct upstream *uc = net->upstream;
	AUTOFREE char *target =
		casemap_cstr(uc != nullptr ? uc->casemap : CASEMAPPING_RFC1459, record.name);
	char *last = nullptr;
	struct err *err = nullptr;
	if (u->store.db != nullptr &&
	    msgstore_last_id(&u->store, net->record.id, target, &last, &err)) {
		xfree(record.detached_internal_msg_id);
		record.detached_internal_msg_id = last;
	} else if (err != nullptr) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(record.name));
		log_info(&u->srv->log, "failed to get last message ID for channel %s: %s", q,
			 err_msg(err));
		err_free(err);
	}
	bool ok = user_store_channel(u, net, &record, nullptr);
	db_channel_free(&record);
	if (!ok) {
		return;
	}
	if (uc != nullptr) {
		upstream_set_auto_detach(uc, channel, 0);
	}
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *dc = u->clients.data[i];
		if (dc->network_id != net->record.id) {
			continue;
		}
		struct irc_msg *part = IRC_MSG("PART", channel, "Detach");
		irc_msg_set_prefix(part, DOWNSTREAM_SELF_PREFIX, "", "");
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
	saved = cmmap_get(&net->channels, channel);
	upstream_set_auto_detach(net->upstream, channel, saved->detach_after_ns / 1000000);
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *dc = u->clients.data[i];
		if (dc->network_id != net->record.id) {
			continue;
		}
		struct user_packet *p = user_packet_new(dc, nullptr);
		user_channel_replay(u, net, dc, p, ch, false);
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
