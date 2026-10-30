// WEBPUSH (soju.im/webpush) from registered downstream connections (soju-rs
// downstream/cmds/webpush.rs)
#include "user/internal.h"

#include "base/alloc.h"
#include "base/utf8.h"
#include "net/url.h"
#include "webpush/webpush.h"

// Like soju-rs, the replies have no source
static struct irc_msg *unprefixed(struct irc_msg *msg) {
	irc_msg_set_prefix(msg, "*", "", "");
	return msg;
}

// Maximum number of Web Push subscriptions per user and network
static constexpr size_t MAX_SUBSCRIPTIONS = 25;
// Maximum number of test notifications being sent at once per user
static constexpr size_t MAX_PENDING_CHECKS = 4;
// A known subscription is only checked again after this long
static constexpr int64_t RECHECK_SECS = 24LL * 3600;

static void fail(struct user_packet *p, const char *code, const char *sub, const char *text) {
	user_packet_add_take(p, IRC_MSG("FAIL", "WEBPUSH", code, sub, text));
}

static void internal_error(struct user_packet *p, const char *sub) {
	fail(p, "INTERNAL_ERROR", sub, "Internal error");
}

static char *check_key(int64_t network_id, const char *endpoint) {
	return xasprintf("%lld %s", (long long)network_id, endpoint);
}

struct list_subs {
	int64_t user_id, network_id;
	db_webpush_sub_vec subs;
	struct err *err;
};
static void list_subs(struct db *db, void *data) {
	struct list_subs *j = data;
	bool ok = db_list_webpush_subscriptions(db, j->user_id, j->network_id, &j->subs, &j->err);
	(void)ok;
}

// The subscriptions of a network, synchronously like soju-rs; false (and
// logged) on error
static bool get_subs(struct user *u, int64_t network_id, db_webpush_sub_vec *out) {
	struct list_subs j = {.user_id = u->record.id, .network_id = network_id};
	db_thread_call(u->srv->db, list_subs, &j);
	if (j.err != nullptr) {
		log_info(&u->srv->log, "failed to fetch Web push subscription: %s", err_msg(j.err));
		err_free(j.err);
		db_webpush_sub_vec_free(&j.subs);
		return false;
	}
	*out = j.subs;
	return true;
}

static const struct db_webpush_subscription *find_sub(const db_webpush_sub_vec *subs,
						      const char *endpoint) {
	for (size_t i = 0; i < subs->len; i++) {
		if (cstr_eq(subs->data[i].endpoint, endpoint)) {
			return &subs->data[i];
		}
	}
	return nullptr;
}

struct sub_job {
	int64_t user_id, network_id;
	struct db_webpush_subscription *sub;
	int64_t delete_id;
	struct err *err;
};
static void store_sub(struct db *db, void *data) {
	struct sub_job *j = data;
	bool ok = j->sub != nullptr ? db_store_webpush_subscription(db, j->user_id, j->network_id,
								    j->sub, &j->err)
				    : db_delete_webpush_subscription(db, j->delete_id, &j->err);
	(void)ok;
}

// A test notification of WEBPUSH REGISTER in flight
struct webpush_check {
	struct server *srv;
	int64_t user_id;
	uint64_t dc_id, check;
	int64_t network_id;
	struct irc_msg *command; // the REGISTER message, for its label
	struct db_webpush_subscription sub;
};

static void check_free(struct webpush_check *c) {
	atomic_fetch_sub(&c->srv->pending_work, 1);
	irc_msg_free(c->command);
	db_webpush_subscription_free(&c->sub);
	xfree(c);
}

// soju-rs web_push_tested: finishes REGISTER once the test notification
// was sent
static void tested(struct user *u, struct user_client *dc, struct webpush_check *c, const char *err,
		   struct user_packet *p) {
	const char *endpoint = c->sub.endpoint;
	AUTOFREE char *key = check_key(c->network_id, endpoint);
	// A later REGISTER of the same endpoint is being checked: it stores the
	// subscription
	uint64_t *current = strmap_get(&u->web_push_checks, key);
	bool superseded = current != nullptr && *current != c->check;
	// UNREGISTER was received meanwhile
	bool cancelled = !superseded && current == nullptr;
	if (!superseded && current != nullptr) {
		xfree(strmap_take(&u->web_push_checks, key));
	}
	if (dc == nullptr) {
		return;
	}
	if (err != nullptr) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(endpoint));
		log_info(&u->srv->log, "failed to send Web push notification to endpoint %s: %s", q,
			 err);
		fail(p, "INVALID_PARAMS", "REGISTER", "Invalid endpoint");
		return;
	}
	if (cancelled) {
		fail(p, "INTERNAL_ERROR", "REGISTER", "Registration cancelled");
		return;
	}
	if (!superseded) {
		// The subscriptions may have changed while the notification was
		// being sent
		db_webpush_sub_vec subs = {0};
		if (!get_subs(u, c->network_id, &subs)) {
			internal_error(p, "REGISTER");
			return;
		}
		const struct db_webpush_subscription *old = find_sub(&subs, endpoint);
		size_t n = subs.len;
		c->sub.id = old != nullptr ? old->id : 0;
		db_webpush_sub_vec_free(&subs);
		if (old == nullptr && n >= MAX_SUBSCRIPTIONS) {
			fail(p, "INTERNAL_ERROR", "REGISTER", "Too many subscriptions");
			return;
		}
		struct sub_job j = {
			.user_id = u->record.id, .network_id = c->network_id, .sub = &c->sub};
		db_thread_call(u->srv->db, store_sub, &j);
		if (j.err != nullptr) {
			log_info(&u->srv->log, "failed to store Web push subscription: %s",
				 err_msg(j.err));
			err_free(j.err);
			internal_error(p, "REGISTER");
			return;
		}
		user_reset_web_push_networks(u);
	}
	user_packet_add_take(p, unprefixed(IRC_MSG("WEBPUSH", "REGISTER", endpoint)));
}

static void test_sent(void *data, enum webpush_result result, const char *err) {
	struct webpush_check *c = data;
	struct user *u = user_lookup(c->srv, c->user_id);
	if (u != nullptr && !u->stopped) {
		struct user_client *dc = user_get_client(u, c->dc_id);
		struct user_packet *p = dc != nullptr ? user_packet_new(dc, c->command) : nullptr;
		const char *e = result == WEBPUSH_SENT      ? nullptr
				: result == WEBPUSH_EXPIRED ? "Web Push subscription expired"
							    : err;
		tested(u, dc, c, e, p);
		if (p != nullptr) {
			for (size_t i = 0; i < p->messages.len; i++) {
				server_set_prefix(u->srv, p->messages.data[i]);
			}
			user_packet_send_take(p);
		}
	}
	check_free(c);
}

static bool same_keys(const struct db_webpush_subscription *a,
		      const struct db_webpush_subscription *b) {
	return cstr_eq(a->key_auth, b->key_auth) && cstr_eq(a->key_p256dh, b->key_p256dh) &&
	       cstr_eq(a->key_vapid, b->key_vapid);
}

// soju-rs sanity_check_web_push_endpoint
static bool sane_endpoint(const char *endpoint, struct err **err) {
	struct url u;
	if (!url_parse(str_from(endpoint), &u, err)) {
		return false;
	}
	bool https = cstr_eq(u.scheme, "https");
	url_free(&u);
	return https || err_set(err, 0, "scheme must be HTTPS");
}

// Sends the test notification of a new or changed subscription; true if
// the reply comes later
static bool start_check(struct user *u, struct user_client *dc, const struct irc_msg *msg,
			struct db_webpush_subscription *sub, struct user_packet *p) {
	struct err *e = nullptr;
	if (!sane_endpoint(sub->endpoint, &e)) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(sub->endpoint));
		log_info(&u->srv->log, "failed to sanity check Web push endpoint %s: %s", q,
			 err_msg(e));
		err_free(e);
		fail(p, "INVALID_PARAMS", "REGISTER", "Invalid endpoint");
		return false;
	}
	AUTOFREE char *key = check_key(dc->network_id, sub->endpoint);
	if (!strmap_has(&u->web_push_checks, key) &&
	    strmap_len(&u->web_push_checks) >= MAX_PENDING_CHECKS) {
		fail(p, "INTERNAL_ERROR", "REGISTER",
		     "Too many pending subscriptions, try again later");
		return false;
	}
	struct webpush_check *c = xcalloc(1, sizeof(*c));
	*c = (struct webpush_check){.srv = u->srv,
				    .user_id = u->record.id,
				    .dc_id = dc->id,
				    .check = u->next_web_push_check++,
				    .network_id = dc->network_id,
				    .command = irc_msg_clone(msg),
				    .sub = *sub};
	*sub = (struct db_webpush_subscription){0};
	uint64_t *id = xmalloc(sizeof(*id));
	*id = c->check;
	strmap_set(&u->web_push_checks, key, id);
	atomic_fetch_add(&u->srv->pending_work, 1);
	// The test notification is sent in the background: the reply comes
	// once it's done
	struct irc_msg *test =
		IRC_MSG("NOTE", "WEBPUSH", "REGISTERED", "Push notifications enabled");
	AUTOFREE char *payload = irc_msg_to_string(test);
	irc_msg_free(test);
	webpush_send(u->srv->web_push, &u->loop->uv, &u->srv->log, c->sub.endpoint, c->sub.key_auth,
		     c->sub.key_p256dh, c->sub.key_vapid, str_from(payload), WEBPUSH_NORMAL,
		     test_sent, c);
	return true;
}

static bool reg(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		struct user_packet *p) {
	const char *endpoint = irc_msg_param(msg, 1);
	struct irc_tags keys = IRC_TAGS_INIT;
	irc_tags_parse(&keys, str_from(irc_msg_param(msg, 2)));
	const char *auth = irc_tags_get(&keys, "auth"), *p256dh = irc_tags_get(&keys, "p256dh");
	if (auth == nullptr || p256dh == nullptr) {
		irc_tags_free(&keys);
		fail(p, "INVALID_PARAMS", "REGISTER", "Missing auth or p256dh key");
		return false;
	}
	struct db_webpush_subscription sub;
	db_webpush_subscription_init(&sub);
	xfree(sub.endpoint);
	xfree(sub.key_auth);
	xfree(sub.key_p256dh);
	xfree(sub.key_vapid);
	sub.endpoint = xstrdup(endpoint);
	sub.key_auth = xstrdup(auth);
	sub.key_p256dh = xstrdup(p256dh);
	sub.key_vapid = xstrdup(u->srv->web_push->vapid_public);
	irc_tags_free(&keys);
	bool later = false;
	db_webpush_sub_vec subs = {0};
	if (!get_subs(u, dc->network_id, &subs)) {
		internal_error(p, "REGISTER");
		goto out;
	}
	const struct db_webpush_subscription *old = find_sub(&subs, endpoint);
	if (old == nullptr) {
		// Refreshing an existing subscription is always allowed
		size_t pending_new = 0;
		const char *k;
		for (size_t it = 0; strmap_next(&u->web_push_checks, &it, &k, nullptr);) {
			AUTOFREE char *prefix = xasprintf("%lld ", (long long)dc->network_id);
			struct str ks = str_from(k);
			if (str_starts_with(ks, str_from(prefix)) &&
			    find_sub(&subs, k + strlen(prefix)) == nullptr) {
				pending_new++;
			}
		}
		if (subs.len + pending_new >= MAX_SUBSCRIPTIONS) {
			fail(p, "INTERNAL_ERROR", "REGISTER", "Too many subscriptions");
			goto out;
		}
	}
	bool update = true;
	if (old != nullptr) {
		// Update the old subscription instead of creating a new one;
		// rate-limit subscription checks
		sub.id = old->id;
		int64_t now = timestamp_now().sec;
		bool stale = !old->updated_at.set || now - old->updated_at.t.sec > RECHECK_SECS;
		update = stale || !same_keys(old, &sub);
	}
	if (update) {
		later = start_check(u, dc, msg, &sub, p);
	} else {
		user_packet_add_take(p, unprefixed(IRC_MSG("WEBPUSH", "REGISTER", endpoint)));
	}
out:
	db_webpush_sub_vec_free(&subs);
	db_webpush_subscription_free(&sub);
	return later;
}

static void unreg(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		  struct user_packet *p) {
	const char *endpoint = irc_msg_param(msg, 1);
	AUTOFREE char *key = check_key(dc->network_id, endpoint);
	xfree(strmap_take(&u->web_push_checks, key));
	db_webpush_sub_vec subs = {0};
	if (!get_subs(u, dc->network_id, &subs)) {
		internal_error(p, "UNREGISTER");
		return;
	}
	user_reset_web_push_networks(u);
	const struct db_webpush_subscription *old = find_sub(&subs, endpoint);
	if (old != nullptr) {
		struct sub_job j = {.delete_id = old->id};
		db_thread_call(u->srv->db, store_sub, &j);
		if (j.err != nullptr) {
			log_info(&u->srv->log, "failed to delete Web push subscription: %s",
				 err_msg(j.err));
			err_free(j.err);
			db_webpush_sub_vec_free(&subs);
			internal_error(p, "UNREGISTER");
			return;
		}
	}
	db_webpush_sub_vec_free(&subs);
	user_packet_add_take(p, unprefixed(IRC_MSG("WEBPUSH", "UNREGISTER", endpoint)));
}

bool user_webpush_command(struct user *u, struct user_client *dc, const struct irc_msg *msg,
			  struct user_packet *p) {
	if (!cap_registry_is_enabled(&dc->caps, "soju.im/webpush")) {
		user_packet_add_take(p, IRC_MSG("421", "*", msg->command, "Unknown command"));
		return false;
	}
	if (msg->params.len < 1) {
		user_packet_add_take(p, IRC_MSG("461", "*", msg->command, "Not enough parameters"));
		return false;
	}
	const char *sub = irc_msg_param(msg, 0);
	if (u->srv->web_push == nullptr) {
		internal_error(p, sub);
		return false;
	}
	if (cstr_eq(sub, "REGISTER") || cstr_eq(sub, "UNREGISTER")) {
		size_t need = cstr_eq(sub, "REGISTER") ? 3 : 2;
		if (msg->params.len < need) {
			user_packet_add_take(
				p, IRC_MSG("461", "*", msg->command, "Not enough parameters"));
			return false;
		}
		if (cstr_eq(sub, "REGISTER")) {
			return reg(u, dc, msg, p);
		}
		unreg(u, dc, msg, p);
		return false;
	}
	fail(p, "INVALID_PARAMS", sub, "Unknown command");
	return false;
}
