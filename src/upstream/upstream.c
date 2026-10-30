#include "upstream/internal.h"

#include <assert.h>
#include <ctype.h>
#include <openssl/rand.h>

#include "base/alloc.h"
#include "base/utf8.h"
#include "irc/numerics.h"
#include "net/identd.h"

static atomic_uint_fast64_t next_id = 1;
static thread_local struct strmap *sessions;

static char *key(uint64_t id) {
	return xasprintf("%llu", (unsigned long long)id);
}

struct upstream *upstream_lookup(uint64_t id) {
	if (sessions == nullptr) {
		return nullptr;
	}
	AUTOFREE char *k = key(id);
	return strmap_get(sessions, k);
}

void upstream_free_batch(void *data) {
	struct upstream_batch *b = data;
	if (b->multiline != nullptr) {
		multiline_batch_free(b->multiline);
		xfree(b->multiline);
	}
	xfree(b->label);
	xfree(b);
}

static void free_pending(void *data) {
	pending_command_vec *v = data;
	for (size_t i = 0; i < v->len; i++) {
		xfree(v->data[i].label);
		irc_msg_free(v->data[i].msg);
	}
	vec_free(v);
	xfree(v);
}

void upstream_set_casemapping(struct upstream *uc, enum casemapping cm) {
	uc->casemap = cm;
	cmmap_set_casemapping(&uc->channels, cm);
	cmmap_set_casemapping(&uc->users, cm);
	cmmap_set_casemapping(&uc->monitored, cm);
	void *v;
	for (size_t it = 0; cmmap_next(&uc->channels, &it, nullptr, &v);) {
		struct upstream_channel *ch = v;
		cmmap_set_casemapping(&ch->members, cm);
	}
}

static void timer_cb(uv_timer_t *timer) {
	struct upstream *uc = timer->data;
	upstream_tick(uc, uv_now(timer->loop));
}

struct upstream *upstream_new(struct server *srv, const struct db_user *u,
			      const struct db_network *n, const struct upstream_events *events) {
	struct upstream *uc = xcalloc(1, sizeof(*uc));
	REF_TRACK(&uc->ref, "upstream");
	uc->id = atomic_fetch_add(&next_id, 1);
	uc->srv = srv;
	uc->loop = loop_current();
	assert(uc->loop != nullptr);
	db_user_copy(&uc->user, u);
	db_network_copy(&uc->network, n);
	if (events != nullptr) {
		uc->events = *events;
	}
	AUTOFREE char *p = xasprintf("user %s: network %s: ", u->username, db_network_get_name(n));
	log_with_prefix(&uc->log, &srv->log, p);
	uc->nick = xstrdup(db_get_nick(u, n));
	uc->username = xstrdup(db_get_username(u, n));
	uc->realname = xstrdup(db_get_realname(u, n));
	uc->hostname = xstrdup("");
	uc->account = xstrdup("");
	uc->server_name = xstrdup("*");
	uc->channel_types = xstrdup(STD_CHANNEL_TYPES);
	uc->status_msg = xstrdup("");
	uc->casemap = CASEMAPPING_RFC1459;
	cap_registry_init(&uc->caps);
	isupport_init(&uc->isupport);
	strmap_init(&uc->batches, upstream_free_batch);
	strmap_init(&uc->pending, free_pending);
	upstream_channels_init(uc);
	cmmap_init(&uc->monitored, uc->casemap, xfree);
	std_channel_modes(&uc->mode_types);
	size_t n_members;
	const struct membership *ms = std_memberships(&n_members);
	for (size_t i = 0; i < n_members; i++) {
		vec_push(&uc->memberships, ms[i]);
	}
	backoffer_init(&uc->reconnect_backoff, 60000, 600000, 60000);
	uc->reconnect_backoff.n = 1;
	backoffer_init(&uc->regain_backoff, 15000, 600000, 10000);
	uc->now = uv_now(&uc->loop->uv);
	uv_timer_init(&uc->loop->uv, &uc->timer);
	uc->timer.data = uc;
	uv_timer_start(&uc->timer, timer_cb, 100, 100);
	if (sessions == nullptr) {
		sessions = xmalloc(sizeof(*sessions));
		strmap_init(sessions, nullptr);
	}
	AUTOFREE char *k = key(uc->id);
	strmap_set(sessions, k, uc);
	return uc;
}

static void clear_ident(struct upstream *uc) {
	if (uc->ident_remote != nullptr && uc->srv->identd != nullptr) {
		identd_delete(uc->srv->identd, uc->ident_remote, uc->ident_local);
	}
	xfree(uc->ident_remote);
	xfree(uc->ident_local);
	uc->ident_remote = uc->ident_local = nullptr;
}

void upstream_clear_session(struct upstream *uc) {
	clear_ident(uc);
	upstream_abort_commands(uc);
	strmap_clear(&uc->batches);
	cmmap_clear(&uc->channels);
	cmmap_clear(&uc->users);
	cmmap_clear(&uc->monitored);
	cap_registry_free(&uc->caps);
	cap_registry_init(&uc->caps);
	isupport_free(&uc->isupport);
	isupport_init(&uc->isupport);
	user_modes_free(&uc->modes);
	irc_msg_vec_free(&uc->delayed);
	uc->registered = uc->attached = uc->got_motd = false;
	uc->sasl_started = uc->sasl_active = false;
	uc->attach_at = uc->command_at = uc->regain_at = 0;
	uc->connect_command = 0;
	uc->rate_enabled = true;
	char **fields[] = {&uc->nick,    &uc->username,      &uc->realname,   &uc->hostname,
			   &uc->account, &uc->channel_types, &uc->status_msg, &uc->server_name};
	const char *values[] = {db_get_nick(&uc->user, &uc->network),
				db_get_username(&uc->user, &uc->network),
				db_get_realname(&uc->user, &uc->network),
				"",
				"",
				STD_CHANNEL_TYPES,
				"",
				"*"};
	for (size_t i = 0; i < sizeof(fields) / sizeof(*fields); i++) {
		char *copy = xstrdup(values[i]);
		xfree(*fields[i]);
		*fields[i] = copy;
	}
	channel_mode_types_free(&uc->mode_types);
	std_channel_modes(&uc->mode_types);
	vec_clear(&uc->memberships);
	size_t n_members;
	const struct membership *ms = std_memberships(&n_members);
	for (size_t i = 0; i < n_members; i++) {
		vec_push(&uc->memberships, ms[i]);
	}
	xfree(uc->ended_command);
	xfree(uc->ended_target);
	uc->ended_command = uc->ended_target = nullptr;
	backoffer_reset(&uc->regain_backoff);
}

static void timer_closed(uv_handle_t *h) {
	struct upstream *uc = h->data;
	upstream_clear_session(uc);
	strmap_free(&uc->batches);
	strmap_free(&uc->pending);
	cmmap_free(&uc->channels);
	cmmap_free(&uc->users);
	cmmap_free(&uc->monitored);
	cap_registry_free(&uc->caps);
	isupport_free(&uc->isupport);
	channel_mode_types_free(&uc->mode_types);
	vec_free(&uc->memberships);
	db_user_free(&uc->user);
	db_network_free(&uc->network);
	xfree(uc->nick);
	xfree(uc->username);
	xfree(uc->hostname);
	xfree(uc->realname);
	xfree(uc->account);
	xfree(uc->channel_types);
	xfree(uc->status_msg);
	xfree(uc->server_name);
	xfree(uc->ended_command);
	xfree(uc->ended_target);
	strvec_free(&uc->event_channels);
	xfree(uc->register_password);
	xfree(uc->sasl_username);
	xfree(uc->sasl_password);
	log_free(&uc->log);
	ref_untrack(&uc->ref);
	xfree(uc);
}

void upstream_free(struct upstream *uc) {
	if (uc == nullptr || uc->closing) {
		return;
	}
	clear_ident(uc);
	upstream_count(uc, false);
	uc->closing = true;
	uc->reconnect = false;
	upstream_abort_commands(uc);
	AUTOFREE char *k = key(uc->id);
	(void)strmap_take(sessions, k);
	if (strmap_len(sessions) == 0) {
		strmap_free(sessions);
		xfree(sessions);
		sessions = nullptr;
	}
	if (uc->conn != nullptr) {
		conn_set_callbacks(uc->conn, &(struct conn_callbacks){0}, nullptr);
		conn_close(uc->conn);
		conn_unref(uc->conn);
		uc->conn = nullptr;
	}
	uv_close((uv_handle_t *)&uc->timer, timer_closed);
}

void upstream_notify_state(struct upstream *uc) {
	if (uc->events.state != nullptr) {
		uc->events.state(uc, uc->events.data);
	}
}

void upstream_emit_take(struct upstream *uc, struct irc_msg *msg, uint64_t dc_id) {
	if (uc->events.message != nullptr) {
		uc->events.message(uc, msg, dc_id, uc->events.data);
	} else {
		irc_msg_free(msg);
	}
}

static void raw_send(struct upstream *uc, struct irc_msg *msg) {
	atomic_fetch_add(&uc->srv->metrics.upstream_out_messages_total, 1);
	if (uc->events.send != nullptr) {
		uc->events.send(uc, msg, uc->events.data);
	} else {
		if (uc->conn != nullptr) {
			conn_send(uc->conn, msg);
		}
		irc_msg_free(msg);
	}
}

static size_t prefix_len(void *data) {
	struct upstream *uc = data;
	return size_add(size_add(strlen(uc->nick), 2),
			size_add(strlen(uc->username) > 10 ? strlen(uc->username) : 10,
				 uc->hostname[0] != '\0' ? strlen(uc->hostname) : 63));
}

static char *batch_ref(void *data) {
	struct upstream *uc = data;
	return xasprintf("%llu", (unsigned long long)++uc->next_batch);
}

void upstream_send_take(struct upstream *uc, struct irc_msg *msg) {
	if (uc->closing) {
		irc_msg_free(msg);
		return;
	}
	if (!cap_registry_is_enabled(&uc->caps, "message-tags")) {
		irc_tags_clear(&msg->tags);
	}
	irc_msg_vec prepared = {0};
	bool multiline = cap_registry_is_enabled(&uc->caps, "batch") &&
			 cap_registry_is_enabled(&uc->caps, MULTILINE_BATCH_TYPE);
	enum text_message_kind kind =
		prepare_text_message(msg, multiline, prefix_len, batch_ref, uc, &prepared);
	if (kind == TEXT_MESSAGE_SINGLE) {
		raw_send(uc, msg);
	} else {
		for (size_t i = 0; i < prepared.len; i++) {
			if (i > 0 && kind == TEXT_MESSAGE_LINES) {
				irc_tags_remove(&prepared.data[i]->tags, "label");
			}
			raw_send(uc, prepared.data[i]);
		}
		vec_free(&prepared);
		irc_msg_free(msg);
	}
}

void upstream_send_labeled_take(struct upstream *uc, uint64_t dc_id, const char *label,
				struct irc_msg *msg) {
	if (cap_registry_is_enabled(&uc->caps, "labeled-response")) {
		AUTOFREE char *l = xasprintf("sd-%llu-%llu-%s", (unsigned long long)dc_id,
					     (unsigned long long)uc->next_label++, label);
		irc_tags_set(&msg->tags, "label", l);
	}
	upstream_send_take(uc, msg);
}

bool upstream_is_channel(const struct upstream *uc, const char *name) {
	return name[0] != '\0' && strchr(uc->channel_types, name[0]) != nullptr;
}

bool upstream_supports_sasl(const struct upstream *uc, const char *mechanism) {
	const char *v = cap_registry_value(&uc->caps, "sasl");
	if (v == nullptr) {
		return false;
	}
	if (v[0] == '\0') {
		return true;
	}
	struct str rest = str_from(v), part;
	while (str_next_field(&rest, ',', &part)) {
		if (str_eq_ignore_ascii_case(part, str_from(mechanism))) {
			return true;
		}
	}
	return false;
}

char *go_errno_message(int uv_error) {
	char *text = xstrdup(uv_strerror(uv_error));
	for (size_t i = 0; text[i] != '\0'; i++) {
		text[i] = (char)tolower((unsigned char)text[i]);
	}
	return text;
}

void upstream_count(struct upstream *uc, bool running) {
	if (uc->counted != running) {
		uc->counted = running;
		if (running) {
			atomic_fetch_add(&uc->srv->metrics.upstreams, 1);
		} else {
			atomic_fetch_sub(&uc->srv->metrics.upstreams, 1);
		}
	}
}

void upstream_disconnect(struct upstream *uc, const char *reason, bool temporary) {
	if (uc->closing) {
		return;
	}
	upstream_count(uc, false);
	// soju-rs resets the backoff when a registered connection ends without
	// error: the server is retried right away
	if (uc->registered && reason[0] == '\0') {
		backoffer_reset(&uc->reconnect_backoff);
	}
	if (uc->conn != nullptr) {
		conn_set_callbacks(uc->conn, &(struct conn_callbacks){0}, nullptr);
		conn_close(uc->conn);
		conn_unref(uc->conn);
		uc->conn = nullptr;
	}
	uc->generation++;
	uc->state = temporary ? UPSTREAM_DISCONNECTED : UPSTREAM_STOPPED;
	if (uc->events.closed != nullptr) {
		uc->events.closed(uc, reason, temporary, uc->events.data);
	}
	upstream_clear_session(uc);
	if (uc->reconnect && temporary) {
		// The delay counts from the start of the last attempt
		uint64_t delay = backoffer_next_delay(&uc->reconnect_backoff);
		uint64_t elapsed = uc->now - uc->last_try;
		uc->retry_at = elapsed >= delay ? uc->now : uc->now + delay - elapsed;
		if (uc->retry_at == 0) {
			uc->retry_at = 1;
		}
	}
	upstream_notify_state(uc);
}

void upstream_tick(struct upstream *uc, uint64_t now_ms) {
	uc->now = now_ms;
	if ((uc->state == UPSTREAM_REGISTERING || uc->state == UPSTREAM_CONNECTING) &&
	    uc->deadline != 0 && now_ms >= uc->deadline) {
		upstream_disconnect(uc,
				    uc->state == UPSTREAM_REGISTERING
					    ? "failed to register: failed to read message: "
					      "registration timed out"
					    : "failed to connect: i/o timeout",
				    true);
		return;
	}
	if (uc->state == UPSTREAM_DISCONNECTED && uc->reconnect && uc->retry_at != 0 &&
	    now_ms >= uc->retry_at) {
		uc->retry_at = 0;
		upstream_connect(uc);
	}
	if (!uc->registered) {
		return;
	}
	if (uc->command_at != 0 && now_ms >= uc->command_at) {
		if (uc->connect_command < uc->network.connect_commands.len) {
			struct irc_msg *msg = irc_msg_parse(
				str_from(uc->network.connect_commands.data[uc->connect_command++]),
				nullptr);
			if (msg != nullptr) {
				upstream_send_take(uc, msg);
			}
			uc->command_at = now_ms + 5000;
		} else {
			uc->command_at = 0;
			uc->attach_at = now_ms + 3000;
		}
	}
	if (uc->attach_at != 0 && now_ms >= uc->attach_at) {
		uc->attach_at = 0;
		uc->attached = true;
		upstream_notify_state(uc);
		irc_msg_vec delayed = uc->delayed;
		uc->delayed = (irc_msg_vec){0};
		for (size_t i = 0; i < delayed.len; i++) {
			upstream_receive_take(uc, delayed.data[i]);
		}
		vec_free(&delayed);
	}
	if (uc->regain_at != 0 && now_ms >= uc->regain_at) {
		uc->regain_at = now_ms + backoffer_next_delay(&uc->regain_backoff);
		const char *desired = db_get_nick(&uc->user, &uc->network);
		AUTOFREE char *a = casemap_cstr(uc->casemap, desired);
		AUTOFREE char *b = casemap_cstr(uc->casemap, uc->nick);
		if (!cstr_eq(a, b)) {
			upstream_send_take(uc, IRC_MSG("NICK", desired));
		} else {
			uc->regain_at = 0;
			backoffer_reset(&uc->regain_backoff);
		}
	}
	void *v;
	for (size_t it = 0; cmmap_next(&uc->channels, &it, nullptr, &v);) {
		struct upstream_channel *ch = v;
		if (ch->detach_deadline != 0 && now_ms >= ch->detach_deadline) {
			ch->detach_deadline = 0;
			if (uc->events.detach != nullptr) {
				uc->events.detach(uc, ch->name, uc->events.data);
			}
		}
	}
}

void upstream_set_auto_detach(struct upstream *uc, const char *channel, uint64_t delay_ms) {
	struct upstream_channel *ch = cmmap_get(&uc->channels, channel);
	if (ch != nullptr) {
		ch->detach_after_ms = delay_ms;
		ch->detach_deadline = delay_ms != 0 ? uc->now + delay_ms : 0;
	}
}
