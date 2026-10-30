// Sending to downstreams: tags, labeled responses, batches and
// supported capabilities (soju-rs downstream/send.rs)
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

static void raw_send(struct downstream *dc, struct irc_msg *msg) {
	atomic_fetch_add(&dc->srv->metrics.downstream_out_messages_total, 1);
	if (dc->view != nullptr) {
		conn_view_send(dc->view, msg);
	} else if (dc->conn != nullptr) {
		conn_send(dc->conn, msg);
	}
	irc_msg_free(msg);
}

static bool tag_allowed(const char *key, const char *value, void *data) {
	(void)value;
	const struct cap_registry *caps = data;
	if (cstr_eq(key, "label")) {
		return cap_registry_is_enabled(caps, "labeled-response");
	}
	if (cstr_eq(key, "time")) {
		return cap_registry_is_enabled(caps, "server-time");
	}
	if (cstr_eq(key, "account")) {
		return cap_registry_is_enabled(caps, "account-tag");
	}
	if (cstr_eq(key, "batch")) {
		return cap_registry_is_enabled(caps, "batch");
	}
	// Enabled by their own capability, like server-time
	if (cstr_eq(key, "soju.im/unread") || cstr_eq(key, "soju.im/mention")) {
		return cap_registry_is_enabled(caps, "soju.im/unread");
	}
	return false;
}

static void send_reply(struct downstream *dc, irc_msg_vec *reply) {
	struct label_ctx *l = dc->label;
	if (l == nullptr || l->label[0] == '\0') {
		for (size_t i = 0; i < reply->len; i++) {
			raw_send(dc, reply->data[i]);
		}
		vec_free(reply);
		return;
	}
	// Only a labeled reply may need buffering
	if (l->has_pending) {
		// create a batch
		dc->last_batch_ref++;
		char batch[24];
		snprintf(batch, sizeof(batch), "%llu", (unsigned long long)dc->last_batch_ref);
		replace_str(&l->batch, batch);
		struct irc_msg *first = l->pending.data[0];
		if (irc_tags_get_or_empty(&first->tags, "batch")[0] == '\0') {
			irc_tags_set(&first->tags, "batch", batch);
		}
		AUTOFREE char *plus = xasprintf("+%s", batch);
		struct irc_msg *open = IRC_MSG("BATCH", plus, "labeled-response");
		irc_tags_set(&open->tags, "label", l->label);
		raw_send(dc, open);
		// send the buffered reply
		for (size_t i = 0; i < l->pending.len; i++) {
			raw_send(dc, l->pending.data[i]);
		}
		vec_free(&l->pending);
		l->has_pending = false;
	}
	if (l->batch[0] != '\0') {
		// send the current reply in the batch
		struct irc_msg *first = reply->data[0];
		if (irc_tags_get_or_empty(&first->tags, "batch")[0] == '\0') {
			irc_tags_set(&first->tags, "batch", l->batch);
		}
		for (size_t i = 0; i < reply->len; i++) {
			raw_send(dc, reply->data[i]);
		}
		vec_free(reply);
	} else {
		// first reply we're sending: buffer it
		l->pending = *reply;
		l->has_pending = true;
		*reply = (irc_msg_vec){0};
	}
}

struct prefix_ctx {
	struct downstream *dc;
	size_t prefix_len;
	bool has_prefix;
};

static size_t prefix_len_cb(void *data) {
	struct prefix_ctx *p = data;
	if (p->has_prefix) {
		return p->prefix_len;
	}
	AUTOFREE char *h = server_hostname(p->dc->srv);
	return strlen(h);
}

static char *batch_ref_cb(void *data) {
	struct prefix_ctx *p = data;
	p->dc->last_batch_ref++;
	return xasprintf("%llu", (unsigned long long)p->dc->last_batch_ref);
}

void downstream_send(struct downstream *dc, struct irc_msg *msg) {
	const struct cap_registry *caps = &dc->caps;
	if (dc->forced_batch != nullptr && !irc_tags_contains(&msg->tags, "batch")) {
		irc_tags_set(&msg->tags, "batch", dc->forced_batch);
	}
	if (!cap_registry_is_enabled(caps, "message-tags")) {
		if (cstr_eq(msg->command, "TAGMSG")) {
			irc_msg_free(msg);
			return;
		}
		irc_tags_retain(&msg->tags, tag_allowed, (void *)(uintptr_t)caps);
	}
	if (!cap_registry_is_enabled(caps, "batch") &&
	    irc_tags_get_or_empty(&msg->tags, "batch")[0] != '\0') {
		irc_tags_remove(&msg->tags, "batch");
	}
	if (cstr_eq(msg->command, "JOIN") && !cap_registry_is_enabled(caps, "extended-join")) {
		irc_msg_truncate_params(msg, 1);
	}
	static const char *const required[][2] = {
		{"SETNAME", "setname"},
		{"CHGHOST", "chghost"},
		{"AWAY", "away-notify"},
		{"ACCOUNT", "account-notify"},
		{"MARKREAD", "draft/read-marker"},
		{"READ", "soju.im/read"},
		{"METADATA", "draft/metadata-2"},
		{"REDACT", "draft/message-redaction"},
	};
	for (size_t i = 0; i < N(required); i++) {
		if (cstr_eq(msg->command, required[i][0]) &&
		    !cap_registry_is_enabled(caps, required[i][1])) {
			irc_msg_free(msg);
			return;
		}
	}
	if (msg->prefix != nullptr && cstr_eq(msg->prefix->name, "*")) {
		// "*" is a sentinel value to simplify upstream message handling
		irc_msg_clear_prefix(msg);
	} else if (msg->prefix != nullptr && cstr_eq(msg->prefix->name, DOWNSTREAM_SELF_PREFIX)) {
		irc_msg_set_prefix(msg, dc->nick, dc->username, dc->hostname);
	}
	if ((msg->prefix == nullptr || msg->prefix->name[0] == '\0') && is_numeric(msg->command)) {
		// Numerics must always carry a source
		server_set_prefix(dc->srv, msg);
	}
	bool multiline = cap_registry_is_enabled(caps, "batch") &&
			 cap_registry_is_enabled(caps, MULTILINE_BATCH_TYPE);
	struct prefix_ctx pc = {.dc = dc};
	if (msg->prefix != nullptr) {
		pc.has_prefix = true;
		pc.prefix_len = irc_prefix_formatted_len(msg->prefix);
	}
	irc_msg_vec out = {0};
	switch (prepare_text_message(msg, multiline, prefix_len_cb, batch_ref_cb, &pc, &out)) {
	case TEXT_MESSAGE_SINGLE: {
		irc_msg_vec one = {0};
		vec_push(&one, msg);
		send_reply(dc, &one);
		irc_msg_vec_free(&out);
		return;
	}
	case TEXT_MESSAGE_BATCH:
		// A multiline batch is a single reply
		send_reply(dc, &out);
		break;
	case TEXT_MESSAGE_LINES:
		for (size_t i = 0; i < out.len; i++) {
			irc_msg_vec one = {0};
			vec_push(&one, out.data[i]);
			send_reply(dc, &one);
		}
		vec_free(&out);
		break;
	}
	irc_msg_free(msg);
}

void downstream_flush_batch(struct downstream *dc) {
	struct label_ctx *l = dc->label;
	if (l == nullptr || l->label[0] == '\0') {
		return;
	}
	if (l->has_pending) {
		irc_tags_set(&l->pending.data[0]->tags, "label", l->label);
		for (size_t i = 0; i < l->pending.len; i++) {
			raw_send(dc, l->pending.data[i]);
		}
		vec_free(&l->pending);
		l->has_pending = false;
	} else if (l->batch[0] != '\0') {
		AUTOFREE char *minus = xasprintf("-%s", l->batch);
		raw_send(dc, IRC_MSG("BATCH", minus));
	} else {
		struct irc_msg *ack = IRC_MSG("ACK");
		irc_tags_set(&ack->tags, "label", l->label);
		raw_send(dc, ack);
	}
}

char *downstream_take_label(struct downstream *dc) {
	if (dc->label == nullptr) {
		return xstrdup("");
	}
	char *l = dc->label->label;
	dc->label->label = xstrdup("");
	return l;
}

void downstream_drop_label(struct downstream *dc) {
	if (dc->label != nullptr) {
		replace_str(&dc->label->label, "");
	}
}

char *downstream_begin_batch(struct downstream *dc, const char *type, const char *const *params,
			     size_t n, const struct irc_tags *tags) {
	dc->last_batch_ref++;
	char *r = xasprintf("%llu", (unsigned long long)dc->last_batch_ref);
	if (cap_registry_is_enabled(&dc->caps, "batch")) {
		AUTOFREE char *plus = xasprintf("+%s", r);
		struct irc_msg *m = IRC_MSG("BATCH", plus, type);
		for (size_t i = 0; i < n; i++) {
			irc_msg_add_param(m, params[i]);
		}
		if (tags != nullptr) {
			irc_tags_copy(&m->tags, tags);
		}
		downstream_send(dc, m);
	}
	return r;
}

void downstream_end_batch(struct downstream *dc, const char *ref) {
	if (cap_registry_is_enabled(&dc->caps, "batch")) {
		AUTOFREE char *minus = xasprintf("-%s", ref);
		downstream_send(dc, IRC_MSG("BATCH", minus));
	}
}

void downstream_send_batch(struct downstream *dc, const char *type, const char *const *params,
			   size_t n, irc_msg_vec *msgs) {
	AUTOFREE char *r = downstream_begin_batch(dc, type, params, n, nullptr);
	for (size_t i = 0; i < msgs->len; i++) {
		irc_tags_set(&msgs->data[i]->tags, "batch", r);
		downstream_send(dc, msgs->data[i]);
	}
	vec_free(msgs);
	downstream_end_batch(dc, r);
}

static bool supports_backlog(const struct irc_msg *msg) {
	return cstr_eq(msg->command, "PRIVMSG") || cstr_eq(msg->command, "NOTICE");
}

static void send_ping(struct downstream *dc, const char *id) {
	AUTOFREE char *p = xasprintf("soju-msgid-%s", id);
	downstream_send(dc, IRC_MSG("PING", p));
}

void downstream_send_message_with_id(struct downstream *dc, struct irc_msg *msg, const char *id) {
	bool backlog = supports_backlog(msg);
	downstream_send(dc, msg);
	if (id[0] == '\0' || !backlog || cap_registry_is_enabled(&dc->caps, "draft/chathistory")) {
		return;
	}
	send_ping(dc, id);
}

void downstream_advance_message_with_id(struct downstream *dc, const struct irc_msg *msg,
					const char *id) {
	if (id[0] == '\0' || !supports_backlog(msg) ||
	    cap_registry_is_enabled(&dc->caps, "draft/chathistory")) {
		return;
	}
	send_ping(dc, id);
}

void downstream_end_sasl(struct downstream *dc, struct irc_msg *msg) {
	if (dc->sasl == nullptr) {
		irc_msg_free(msg);
		return;
	}
	downstream_sasl_free(dc->sasl);
	dc->sasl = nullptr;
	if (msg == nullptr) {
		msg = IRC_MSG(RPL_SASLSUCCESS, dc->nick, "SASL authentication successful");
	}
	downstream_send(dc, msg);
}

static void service_message(struct downstream *dc, const char *cmd, const char *text) {
	struct irc_msg *m = IRC_MSG(cmd, dc->nick, text);
	irc_msg_set_prefix(m, SERVICE_NICK, SERVICE_NICK, SERVICE_NICK);
	downstream_send(dc, m);
}

void downstream_send_service_notice(struct downstream *dc, const char *text) {
	service_message(dc, "NOTICE", text);
}

void downstream_send_service_privmsg(struct downstream *dc, const char *text) {
	service_message(dc, "PRIVMSG", text);
}

// Capabilities

void downstream_set_supported_cap(struct downstream *dc, const char *name, const char *value) {
	const char *old = cap_registry_value(&dc->caps, name);
	bool changed = old == nullptr || !cstr_eq(old, value);
	cap_registry_set_available(&dc->caps, name, value);
	if (!cap_registry_is_enabled(&dc->caps, "cap-notify") || !changed) {
		return;
	}
	AUTOFREE char *cap = value[0] != '\0' && dc->cap_version >= 302
				     ? xasprintf("%s=%s", name, value)
				     : xstrdup(name);
	struct irc_msg *m = IRC_MSG("CAP", dc->nick, "NEW", cap);
	server_set_prefix(dc->srv, m);
	downstream_send(dc, m);
}

void downstream_unset_supported_cap(struct downstream *dc, const char *name) {
	bool had = cap_registry_is_available(&dc->caps, name);
	cap_registry_del(&dc->caps, name);
	if (!cap_registry_is_enabled(&dc->caps, "cap-notify") || !had) {
		return;
	}
	struct irc_msg *m = IRC_MSG("CAP", dc->nick, "DEL", name);
	server_set_prefix(dc->srv, m);
	downstream_send(dc, m);
}

void downstream_update_casemapping(struct downstream *dc, enum casemapping cm) {
	dc->casemap = cm;
	char *n = casemap_cstr(cm, dc->nick);
	xfree(dc->nick_cm);
	dc->nick_cm = n;
	cmmap_set_casemapping(&dc->monitored, cm);
}
