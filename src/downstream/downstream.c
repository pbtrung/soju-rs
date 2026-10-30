// Downstream connections: lifecycle, the table of a loop's downstreams
// and views (soju-rs downstream/mod.rs)
#include "downstream/internal.h"

#include <openssl/evp.h>
#include <stdio.h>
#include <string.h>

#include "auth/auth.h"
#include "base/alloc.h"
#include "base/base64.h"
#include "base/utf8.h"
#include "db/db.h"
#include "irc/genmsg.h"
#include "irc/ircutil.h"
#include "irc/numerics.h"
#include "net/url.h"
#include "user/user.h"

// their downstream up here)
static thread_local struct strmap *table;
static struct downstream *downstream_new(struct server *srv, struct conn *c,
					 const struct conn_info *info,
					 const struct downstream_events *events);

static void id_key(char *buf, size_t n, uint64_t id) {
	snprintf(buf, n, "%llu", (unsigned long long)id);
}

struct downstream *downstream_lookup(uint64_t id) {
	if (table == nullptr) {
		return nullptr;
	}
	char k[24];
	id_key(k, sizeof(k), id);
	return strmap_get(table, k);
}

static void table_add(struct downstream *dc) {
	if (table == nullptr) {
		table = xmalloc(sizeof(*table));
		strmap_init(table, nullptr);
	}
	char k[24];
	id_key(k, sizeof(k), dc->id);
	strmap_set(table, k, dc);
	atomic_fetch_add(&dc->srv->metrics.downstreams, 1);
}

static void table_remove(struct downstream *dc) {
	char k[24];
	id_key(k, sizeof(k), dc->id);
	(void)strmap_take(table, k);
	atomic_fetch_sub(&dc->srv->metrics.downstreams, 1);
	if (strmap_len(table) == 0) {
		strmap_free(table);
		xfree(table);
		table = nullptr;
	}
}

// Always-supported downstream capabilities
static const char *const PERMANENT_CAPS[][2] = {
	{"batch", ""},
	{"cap-notify", ""},
	{"echo-message", ""},
	{"invite-notify", ""},
	{"no-implicit-names", ""},
	{"server-time", ""},
	{"setname", ""},
	{"draft/extended-isupport", ""},
	{"draft/metadata-2", "before-connect,max-keys=0,max-value-bytes=1"},
	{"draft/no-implicit-names", ""},
	{"draft/pre-away", ""},
	{"draft/read-marker", ""},
	{"soju.im/account-required", ""},
	{"soju.im/bouncer-networks", ""},
	{"soju.im/bouncer-networks-notify", ""},
	{"soju.im/multiplex", ""},
	{"soju.im/no-implicit-names", ""},
	{"soju.im/read", ""},
	{"soju.im/unread", ""},
	{"soju.im/webpush", ""},
};

static void registration_free(struct registration *r) {
	if (r == nullptr) {
		return;
	}
	xfree(r->nick);
	xfree(r->username);
	if (r->pass != nullptr) {
		explicit_bzero(r->pass, strlen(r->pass));
	}
	xfree(r->pass);
	xfree(r->network_name);
	xfree(r->auth_username);
	xfree(r);
}

void downstream_label_free(struct label_ctx *l) {
	if (l == nullptr) {
		return;
	}
	xfree(l->label);
	xfree(l->batch);
	irc_msg_vec_free(&l->pending);
	xfree(l);
}

static void free_batch(void *p) {
	struct multiline_batch *b = p;
	if (b != nullptr) {
		multiline_batch_free(b);
		xfree(b);
	}
}

static void on_timer_closed(uv_handle_t *h) {
	struct downstream *dc = h->data;
	isupport_free(&dc->isupport);
	cap_registry_free(&dc->caps);
	cmmap_free(&dc->monitored);
	strmap_free(&dc->metadata_subs);
	strmap_free(&dc->multiline_batches);
	downstream_sasl_free(dc->sasl);
	registration_free(dc->reg);
	downstream_label_free(dc->label);
	irc_msg_vec_free(&dc->pending_input);
	xfree(dc->remote_addr);
	strbuf_free(&dc->peer_cert);
	xfree(dc->client_name);
	xfree(dc->nick);
	xfree(dc->nick_cm);
	xfree(dc->realname);
	xfree(dc->username);
	xfree(dc->hostname);
	xfree(dc->account);
	xfree(dc->away);
	log_free(&dc->log);
	ref_untrack(&dc->ref);
	xfree(dc);
}

// Removes dc from its loop and frees it (the connection is closed with a
// graceful shutdown)
static void downstream_free(struct downstream *dc) {
	table_remove(dc);
	conn_view_free(dc->view);
	if (dc->conn != nullptr) {
		if (!dc->has_parent) {
			conn_shutdown(dc->conn);
			conn_set_callbacks(dc->conn, &(struct conn_callbacks){0}, nullptr);
		}
		conn_unref(dc->conn);
	}
	uv_close((uv_handle_t *)&dc->register_timer, on_timer_closed);
}

void downstream_close(struct downstream *dc) {
	if (dc->closed) {
		return;
	}
	dc->closed = true;
	if (!dc->has_parent && table != nullptr) {
		// Closing children removes table entries, so collect IDs first.
		VEC(uint64_t) ids = {0};
		void *v;
		for (size_t it = 0; strmap_next(table, &it, nullptr, &v);) {
			struct downstream *child = v;
			if (child->has_parent && child->parent_id == dc->id) {
				vec_push(&ids, child->id);
			}
		}
		for (size_t i = 0; i < ids.len; i++) {
			struct downstream *child = downstream_lookup(ids.data[i]);
			if (child != nullptr) {
				downstream_close(child);
			}
		}
		vec_free(&ids);
	}
	if (dc->events.closed != nullptr) {
		dc->events.closed(dc, dc->events.data);
	}
	downstream_free(dc);
}

void downstream_close_server(struct server *srv) {
	VEC(uint64_t) ids = {0};
	if (table != nullptr) {
		void *v;
		for (size_t it = 0; strmap_next(table, &it, nullptr, &v);) {
			struct downstream *dc = v;
			if (dc->srv == srv) {
				vec_push(&ids, dc->id);
			}
		}
	}
	for (size_t i = 0; i < ids.len; i++) {
		struct downstream *dc = downstream_lookup(ids.data[i]);
		if (dc != nullptr) {
			downstream_close(dc);
		}
	}
	vec_free(&ids);
}

void downstream_registration_done(struct downstream *dc) {
	registration_free(dc->reg);
	dc->reg = nullptr;
}

void downstream_end_command(struct downstream *dc) {
	downstream_flush_batch(dc);
	downstream_drop_label(dc);
}

struct downstream *downstream_find_attached(uint64_t parent_id, int64_t network_id) {
	if (table == nullptr) {
		return nullptr;
	}
	void *v;
	for (size_t it = 0; strmap_next(table, &it, nullptr, &v);) {
		struct downstream *dc = v;
		if (dc->has_parent && dc->parent_id == parent_id && dc->network_id == network_id &&
		    !dc->closed) {
			return dc;
		}
	}
	return nullptr;
}

static void view_closed(void *data) {
	struct downstream *dc = data;
	downstream_close(dc);
}

void downstream_detach(struct downstream *dc) {
	if (dc->view != nullptr) {
		conn_view_close_quietly(dc->view);
	}
	downstream_close(dc);
}

struct downstream *downstream_attach(struct downstream *parent, int64_t network_id) {
	struct downstream *old = downstream_find_attached(parent->id, network_id);
	if (old != nullptr) {
		downstream_detach(old);
	}
	struct conn_info info = {.remote_addr = parent->remote_addr,
				 .has_remote_ip = parent->has_remote_ip,
				 .remote_ip = parent->remote_ip,
				 .peer_cert = parent->has_peer_cert ? &parent->peer_cert : nullptr};
	conn_ref(parent->conn);
	struct downstream *dc = downstream_new(parent->srv, parent->conn, &info, &parent->events);
	dc->has_parent = true;
	dc->parent_id = parent->id;
	dc->has_network = true;
	dc->network_id = network_id;
	dc->registered = true;
	dc->user_id = parent->user_id;
	dc->user_loop = parent->user_loop;
	dc->impersonating = parent->impersonating;
	dc->cap_version = parent->cap_version;
	uv_timer_stop(&dc->register_timer);
	replace_str(&dc->client_name, parent->client_name);
	replace_str(&dc->nick, parent->nick);
	replace_str(&dc->nick_cm, parent->nick_cm);
	replace_str(&dc->username, parent->username);
	replace_str(&dc->realname, parent->realname);
	if (parent->away != nullptr) {
		dc->away = xstrdup(parent->away);
	}
	cap_registry_free(&dc->caps);
	cap_registry_init(&dc->caps);
	const char *key;
	void *v;
	for (size_t it = 0; strmap_next(&parent->caps.available, &it, &key, &v);) {
		cap_registry_set_available(&dc->caps, key, v);
	}
	for (size_t it = 0; strmap_next(&parent->caps.enabled, &it, &key, nullptr);) {
		if (!cstr_eq(key, "soju.im/multiplex") &&
		    !cstr_eq(key, "soju.im/bouncer-networks-notify")) {
			cap_registry_set_enabled(&dc->caps, key, true);
		}
	}
	AUTOFREE char *net = xasprintf("%lld", (long long)network_id);
	dc->view = conn_view_new(parent->conn, "soju.im/network", net, view_closed, dc);
	return dc;
}

void downstream_resume(struct downstream *dc) {
	if (dc->waiting_input) {
		dc->waiting_input = false;
		conn_input_done(dc->conn);
	}
	if (dc->conn != nullptr && conn_is_closed(dc->conn)) {
		downstream_close(dc);
		return;
	}
	downstream_maybe_register(dc);
	downstream_drain_input(dc);
}

void downstream_resume_input(struct downstream *dc) {
	dc->busy = false;
	downstream_resume(dc);
}

static void on_closed(struct conn *c, const struct err *err, void *data) {
	(void)c;
	struct downstream *dc = data;
	if (err != nullptr) {
		log_info(&dc->log, "failed to read IRC command: %s", err_msg(err));
	}
	downstream_close(dc);
}

static void on_register_timeout(uv_timer_t *t) {
	struct downstream *dc = t->data;
	struct irc_msg *m = IRC_MSG("ERROR", "Connection registration timed out");
	downstream_reply_and_close(dc, m);
}

static struct downstream *downstream_new(struct server *srv, struct conn *c,
					 const struct conn_info *info,
					 const struct downstream_events *events) {
	struct downstream *dc = xcalloc(1, sizeof(*dc));
	REF_TRACK(&dc->ref, "downstream");
	dc->id = atomic_fetch_add(&srv->next_downstream_id, 1);
	dc->srv = srv;
	dc->conn = c;
	if (events != nullptr) {
		dc->events = *events;
	} else {
		dc->events = (struct downstream_events){.registered = user_downstream_registered,
							.message = user_downstream_message,
							.closed = user_downstream_closed,
							.updated = user_downstream_updated};
	}
	AUTOFREE char *q = utf8_debug_cstr(str_from(info->remote_addr));
	AUTOFREE char *prefix = xasprintf("downstream %s: ", q);
	log_with_prefix(&dc->log, &srv->log, prefix);
	dc->remote_addr = xstrdup(info->remote_addr);
	dc->has_remote_ip = info->has_remote_ip;
	dc->remote_ip = info->remote_ip;
	if (info->peer_cert != nullptr) {
		dc->has_peer_cert = true;
		strbuf_append_str(&dc->peer_cert, strbuf_str(info->peer_cert));
	}
	char *host, *port;
	if (split_host_port(str_from(info->remote_addr), &host, &port, nullptr)) {
		xfree(port);
		dc->hostname = host;
	} else {
		dc->hostname = xstrdup(info->remote_addr);
	}
	dc->client_name = xstrdup("");
	dc->nick = xstrdup("*");
	dc->nick_cm = xstrdup("*");
	dc->realname = xstrdup("");
	dc->username = xstrdup("~u");
	dc->account = xstrdup("");
	cap_registry_init(&dc->caps);
	isupport_init(&dc->isupport);
	strmap_init(&dc->multiline_batches, free_batch);
	cmmap_init(&dc->monitored, CASEMAPPING_ASCII, nullptr);
	strmap_init(&dc->metadata_subs, nullptr);
	dc->casemap = CASEMAPPING_ASCII;
	dc->reg = xcalloc(1, sizeof(*dc->reg));
	dc->reg->nick = xstrdup("");
	dc->reg->username = xstrdup("");
	dc->reg->pass = xstrdup("");
	dc->reg->network_name = xstrdup("");
	dc->reg->auth_username = xstrdup("");

	struct server_config *cfg = server_config_get(srv);
	for (size_t i = 0; i < N(PERMANENT_CAPS); i++) {
		cap_registry_set_available(&dc->caps, PERMANENT_CAPS[i][0], PERMANENT_CAPS[i][1]);
	}
	struct strbuf mechs = STRBUF_INIT;
	if (cfg->plain_auth != PLAIN_AUTH_NONE) {
		strbuf_append_cstr(&mechs, "PLAIN");
	}
	if (cfg->oauth2 != nullptr) {
		strbuf_printf(&mechs, "%sOAUTHBEARER", mechs.len > 0 ? "," : "");
	}
	if (cfg->cfg.basic.client_cert_auth && dc->has_peer_cert) {
		strbuf_printf(&mechs, "%sEXTERNAL", mechs.len > 0 ? "," : "");
	}
	cap_registry_set_available(&dc->caps, "sasl", strbuf_cstr(&mechs));
	strbuf_free(&mechs);
	// TODO (upstream): only enable chathistory after authentication, once
	// the user's message store is known to support it
	if (cfg->cfg.basic.msg_store_driver != MSG_STORE_MEMORY) {
		cap_registry_set_available(&dc->caps, "draft/chathistory", "");
		cap_registry_set_available(&dc->caps, "soju.im/search", "");
	}
	if (cfg->cfg.basic.client_cert_auth) {
		cap_registry_set_available(&dc->caps, "soju.im/client-cert", "");
	}
	server_config_unref(cfg);

	uv_timer_init(conn_stream(c)->loop, &dc->register_timer);
	dc->register_timer.data = dc;
	table_add(dc);
	return dc;
}

void downstream_serve(struct server *srv, struct conn *c, const struct conn_info *info,
		      const struct downstream_events *events) {
	struct downstream *dc = downstream_new(srv, c, info, events);
	conn_set_callbacks(
		c, &(struct conn_callbacks){.message = downstream_on_message, .closed = on_closed},
		dc);
	if (atomic_load(&srv->shutting_down)) {
		downstream_send(dc, IRC_MSG("ERROR", "Server is shutting down"));
		downstream_close(dc);
		return;
	}
	uv_timer_start(&dc->register_timer, on_register_timeout, DOWNSTREAM_REGISTER_TIMEOUT_MS, 0);
}
