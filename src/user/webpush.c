// Web Push notifications for a user's networks (soju-rs user/webpush.rs and
// the notification part of upstream/messages.rs)
#include "user/internal.h"

#include "base/alloc.h"
#include "base/utf8.h"
#include "webpush/webpush.h"

// A highlight is pushed if its buffer isn't read within this delay
static constexpr uint64_t PUSH_DELAY_MS = 5000;
// Subscriptions failing for any reason are deleted once this old
static constexpr int64_t PUSH_PRUNE_SECS = 30LL * 24 * 3600;

// A highlight or direct message waiting for PUSH_DELAY_MS
struct push_delay {
	uv_timer_t timer;
	struct server *srv;
	int64_t user_id, net_id;
	struct irc_msg *msg;
	char *buffer, *target_cm;
	struct timestamp time;
};

void user_web_push_init(struct user *u) {
	strmap_init(&u->web_push_checks, xfree);
	u->next_web_push_check = 1;
}

static void delay_closed(uv_handle_t *h) {
	struct push_delay *d = h->data;
	atomic_fetch_sub(&d->srv->pending_work, 1);
	irc_msg_free(d->msg);
	xfree(d->buffer);
	xfree(d->target_cm);
	xfree(d);
}

static void delay_cancel(struct push_delay *d) {
	uv_timer_stop(&d->timer);
	uv_close((uv_handle_t *)&d->timer, delay_closed);
}

void user_web_push_stop(struct user *u) {
	for (size_t i = 0; i < u->push_delays.len; i++) {
		delay_cancel(u->push_delays.data[i]);
	}
	vec_free(&u->push_delays);
	vec_free(&u->web_push_networks);
	strmap_free(&u->web_push_checks);
}

struct list_networks {
	int64_t user_id;
	i64vec ids;
	struct err *err;
};
static void list_networks(struct db *db, void *data) {
	struct list_networks *j = data;
	bool ok = db_list_webpush_networks(db, j->user_id, &j->ids, &j->err);
	(void)ok;
}

bool user_has_web_push_subscriptions(struct user *u, int64_t net_id) {
	if (u->srv->web_push == nullptr) {
		return false;
	}
	if (!u->web_push_networks_loaded) {
		struct list_networks j = {.user_id = u->record.id};
		db_thread_call(u->srv->db, list_networks, &j);
		if (j.err != nullptr) {
			log_info(&u->srv->log, "failed to list Web push subscriptions: %s",
				 err_msg(j.err));
			err_free(j.err);
			vec_free(&j.ids);
			return true;
		}
		vec_free(&u->web_push_networks);
		u->web_push_networks.data = j.ids.data;
		u->web_push_networks.len = j.ids.len;
		u->web_push_networks.cap = j.ids.cap;
		u->web_push_networks_loaded = true;
	}
	for (size_t i = 0; i < u->web_push_networks.len; i++) {
		if (u->web_push_networks.data[i] == net_id) {
			return true;
		}
	}
	return false;
}

void user_reset_web_push_networks(struct user *u) {
	u->web_push_networks_loaded = false;
	vec_free(&u->web_push_networks);
}

// Deletes a subscription in the background, logging the outcome
struct delete_sub {
	struct server *srv;
	int64_t id;
	char *endpoint;
	bool expired; // else pruned
	struct err *err;
};
static void delete_run(struct db *db, void *data) {
	struct delete_sub *j = data;
	bool ok = db_delete_webpush_subscription(db, j->id, &j->err);
	(void)ok;
}
static void delete_done(void *data) {
	struct delete_sub *j = data;
	AUTOFREE char *q = utf8_debug_cstr(str_from(j->endpoint));
	const char *what = j->expired ? "expired" : "pruned";
	if (j->err != nullptr) {
		log_info(&j->srv->log, "failed to delete %s Web Push subscription %s: %s", what, q,
			 err_msg(j->err));
	} else if (j->expired) {
		log_debug(&j->srv->log, "deleted expired Web Push subscription %s", q);
	} else {
		log_info(&j->srv->log, "deleted pruned Web Push subscription %s", q);
	}
	err_free(j->err);
	xfree(j->endpoint);
	xfree(j);
}
static void delete_subscription(struct server *srv, const struct db_webpush_subscription *s,
				bool expired) {
	struct delete_sub *j = xcalloc(1, sizeof(*j));
	*j = (struct delete_sub){
		.srv = srv, .id = s->id, .endpoint = xstrdup(s->endpoint), .expired = expired};
	server_db(srv, delete_run, delete_done, j);
}

// Sends a notification to subscriptions, one after the other (soju-rs
// send_web_push_to), deleting the expired ones
struct push_batch {
	struct server *srv;
	char *payload;
	enum webpush_urgency urgency;
	db_webpush_sub_vec subs;
	size_t next;
};

static void batch_next(struct push_batch *b);

static void batch_sent(void *data, enum webpush_result result, const char *err) {
	struct push_batch *b = data;
	const struct db_webpush_subscription *s = &b->subs.data[b->next];
	if (result == WEBPUSH_EXPIRED) {
		delete_subscription(b->srv, s, true);
	} else if (result == WEBPUSH_FAILED) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(s->endpoint));
		log_info(&b->srv->log, "failed to send Web push notification to endpoint %s: %s", q,
			 err);
		// If it failed for any reason and is old, delete it
		int64_t now = timestamp_now().sec;
		if (!s->updated_at.set || now - s->updated_at.t.sec > PUSH_PRUNE_SECS) {
			delete_subscription(b->srv, s, false);
		}
	}
	b->next++;
	batch_next(b);
}

static void batch_next(struct push_batch *b) {
	if (b->next >= b->subs.len || b->srv->web_push == nullptr ||
	    atomic_load(&b->srv->shutting_down)) {
		atomic_fetch_sub(&b->srv->pending_work, 1);
		xfree(b->payload);
		db_webpush_sub_vec_free(&b->subs);
		xfree(b);
		return;
	}
	const struct db_webpush_subscription *s = &b->subs.data[b->next];
	webpush_send(b->srv->web_push, &loop_current()->uv, &b->srv->log, s->endpoint, s->key_auth,
		     s->key_p256dh, s->key_vapid, str_from(b->payload), b->urgency, batch_sent, b);
}

// Takes msg and subs
static void send_to(struct server *srv, db_webpush_sub_vec *subs, struct irc_msg *msg) {
	struct push_batch *b = xcalloc(1, sizeof(*b));
	b->srv = srv;
	atomic_fetch_add(&srv->pending_work, 1);
	b->subs = *subs;
	*subs = (db_webpush_sub_vec){0};
	const char *cmd = msg->command;
	b->urgency = cstr_eq(cmd, "PRIVMSG") || cstr_eq(cmd, "NOTICE") || cstr_eq(cmd, "INVITE")
			     ? WEBPUSH_HIGH
			     : WEBPUSH_NORMAL;
	b->payload = irc_msg_to_string(msg);
	irc_msg_free(msg);
	batch_next(b);
}

// Lists a network's subscriptions, then sends msg to them
struct broadcast {
	struct server *srv;
	int64_t user_id, net_id;
	struct irc_msg *msg;
	// highlights: checked against the read receipt first
	bool delayed;
	char *buffer, *target_cm;
	struct timestamp time;
	bool read;
	db_webpush_sub_vec subs;
	struct err *err, *receipt_err;
};

static void broadcast_run(struct db *db, void *data) {
	struct broadcast *j = data;
	if (j->delayed) {
		struct db_read_receipt r;
		bool found = false;
		if (!db_get_read_receipt(db, j->net_id, j->target_cm, &r, &found,
					 &j->receipt_err)) {
			return;
		}
		if (found) {
			j->read = r.timestamp.set && timestamp_cmp(r.timestamp.t, j->time) >= 0;
			db_read_receipt_free(&r);
		}
		if (j->read) {
			return;
		}
	}
	bool ok = db_list_webpush_subscriptions(db, j->user_id, j->net_id, &j->subs, &j->err);
	(void)ok;
}

static void broadcast_done(void *data) {
	struct broadcast *j = data;
	if (j->receipt_err != nullptr) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(j->buffer));
		log_info(&j->srv->log, "failed to get the read receipt for %s: %s", q,
			 err_msg(j->receipt_err));
	} else if (j->err != nullptr) {
		log_info(&j->srv->log, "failed to list Web push subscriptions: %s",
			 err_msg(j->err));
	} else if (!j->read && j->subs.len > 0) {
		if (j->delayed) {
			// The notification to dismiss when the buffer gets read
			struct user *u = user_lookup(j->srv, j->user_id);
			struct user_network *net =
				u != nullptr ? user_get_network(u, j->net_id) : nullptr;
			if (net != nullptr) {
				struct timestamp *t = xmalloc(sizeof(*t));
				*t = j->time;
				cmmap_set(&net->push_targets, j->buffer, t);
			}
		}
		send_to(j->srv, &j->subs, j->msg);
		j->msg = nullptr;
	}
	irc_msg_free(j->msg);
	db_webpush_sub_vec_free(&j->subs);
	err_free(j->err);
	err_free(j->receipt_err);
	xfree(j->buffer);
	xfree(j->target_cm);
	xfree(j);
}

void user_broadcast_web_push(struct user *u, int64_t net_id, struct irc_msg *msg) {
	if (!user_has_web_push_subscriptions(u, net_id)) {
		irc_msg_free(msg);
		return;
	}
	struct broadcast *j = xcalloc(1, sizeof(*j));
	*j = (struct broadcast){
		.srv = u->srv, .user_id = u->record.id, .net_id = net_id, .msg = msg};
	server_db(u->srv, broadcast_run, broadcast_done, j);
}

static void delay_fired(uv_timer_t *t) {
	struct push_delay *d = t->data;
	struct user *u = user_lookup(d->srv, d->user_id);
	if (u != nullptr) {
		for (size_t i = 0; i < u->push_delays.len; i++) {
			if (u->push_delays.data[i] == d) {
				u->push_delays.data[i] = u->push_delays.data[--u->push_delays.len];
				break;
			}
		}
	}
	if (u != nullptr && !u->stopped) {
		struct broadcast *j = xcalloc(1, sizeof(*j));
		*j = (struct broadcast){.srv = d->srv,
					.user_id = d->user_id,
					.net_id = d->net_id,
					.msg = d->msg,
					.delayed = true,
					.buffer = d->buffer,
					.target_cm = d->target_cm,
					.time = d->time};
		d->msg = nullptr;
		d->buffer = d->target_cm = nullptr;
		server_db(d->srv, broadcast_run, broadcast_done, j);
	}
	uv_close((uv_handle_t *)&d->timer, delay_closed);
}

struct get_target {
	int64_t net_id;
	const char *name;
	struct db_message_target mt;
	struct err *err;
};
static void get_target(struct db *db, void *data) {
	struct get_target *j = data;
	if (!db_get_message_target(db, j->net_id, j->name, &j->mt, &j->err)) {
		j->mt = (struct db_message_target){0};
	}
}

void user_web_push_message(struct user *u, struct user_network *net, const struct irc_msg *msg,
			   const char *buffer, bool direct, bool self_msg) {
	bool is_text = cstr_eq(msg->command, "PRIVMSG") || cstr_eq(msg->command, "NOTICE");
	if (!is_text || self_msg || buffer[0] == '\0') {
		return;
	}
	// Highlights count in known channels only; detached ones without
	// relaying aren't pushed
	const struct db_channel *ch = cmmap_get(&net->channels, buffer);
	bool highlight = ch != nullptr && user_network_highlight(u, net, msg);
	bool detached = ch != nullptr && ch->detached && ch->relay_detached == FILTER_NONE;
	if (detached || !(highlight || direct) ||
	    !user_has_web_push_subscriptions(u, net->record.id)) {
		return;
	}
	enum casemapping cm =
		net->upstream != nullptr ? net->upstream->casemap : CASEMAPPING_RFC1459;
	AUTOFREE char *source_cm = casemap_cstr(cm, irc_msg_prefix_name(msg));
	struct get_target j = {.net_id = net->record.id, .name = source_cm};
	db_thread_call(u->srv->db, get_target, &j);
	if (j.err != nullptr) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(source_cm));
		log_info(&u->srv->log, "failed to get the message target for %s: %s", q,
			 err_msg(j.err));
		err_free(j.err);
	}
	bool muted = j.mt.muted || j.mt.blocked;
	db_message_target_free(&j.mt);
	if (muted) {
		return;
	}
	struct push_delay *d = xcalloc(1, sizeof(*d));
	*d = (struct push_delay){.srv = u->srv,
				 .user_id = u->record.id,
				 .net_id = net->record.id,
				 .msg = irc_msg_clone(msg),
				 .buffer = xstrdup(buffer),
				 .target_cm = casemap_cstr(cm, buffer)};
	if (!parse_server_time(str_from(irc_tags_get_or_empty(&msg->tags, "time")), &d->time)) {
		d->time = timestamp_now();
	}
	atomic_fetch_add(&u->srv->pending_work, 1);
	uv_timer_init(&u->loop->uv, &d->timer);
	d->timer.data = d;
	uv_timer_start(&d->timer, delay_fired, PUSH_DELAY_MS, 0);
	vec_push(&u->push_delays, d);
}

void user_web_push_read(struct user *u, struct user_network *net, const char *target,
			const char *bound, struct db_time receipt) {
	struct timestamp *t = cmmap_get(&net->push_targets, target);
	if (t == nullptr) {
		return;
	}
	// TODO (upstream): only broadcast if draft/read-marker has been
	// negotiated
	if (receipt.set && timestamp_cmp(receipt.t, *t) >= 0) {
		(void)cmmap_del(&net->push_targets, target);
	}
	user_broadcast_web_push(u, net->record.id, IRC_MSG("MARKREAD", target, bound));
}
