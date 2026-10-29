// Downstream connections (soju-rs downstream/mod.rs, send.rs, caps.rs,
// registration.rs)
#include "downstream/downstream.h"

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

// Downstreams of this loop by ID (rule 2: asynchronous completions look
// their downstream up here)
static thread_local struct strmap *table;
static void drain_input(struct downstream *dc);
static void on_closed(struct conn *c, const struct err *err, void *data);
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
	{"soju.im/webpush", ""},
};

// Capabilities which require support from the upstream server
static const char *const PASSTHROUGH_CAPS[][2] = {
	{"account-notify", ""},
	{"account-tag", ""},
	{"away-notify", ""},
	{"chghost", ""},
	{"extended-join", ""},
	{"extended-monitor", ""},
	{"labeled-response", ""},
	{"message-tags", ""},
	{"multi-prefix", ""},
	{"draft/extended-monitor", ""},
	{"draft/message-redaction", ""},
};

static const char *const PERMANENT_ISUPPORT[] = {"soju.im/SAFERATE", "draft/ACCOUNTREQUIRED"};

static const char *const PASSTHROUGH_ISUPPORT[] = {
	"ACCOUNTEXTBAN", "AWAYLEN",    "BOT",           "CASEMAPPING", "CHANLIMIT", "CHANMODES",
	"CHANNELLEN",    "CHANTYPES",  "CLIENTTAGDENY", "ELIST",       "EXCEPTS",   "EXTBAN",
	"HOSTLEN",       "INVEX",      "KICKLEN",       "LINELEN",     "MAXLIST",   "MAXTARGETS",
	"MODES",         "MONITOR",    "NAMELEN",       "NETWORK",     "NICKLEN",   "PREFIX",
	"SAFELIST",      "STATUSMSG",  "TARGMAX",       "TOPICLEN",    "USERLEN",   "UTF8ONLY",
	"WHOX",          "draft/ICON",
};

#define N(a) (sizeof(a) / sizeof((a)[0]))

static void replace_str(char **field, const char *value) {
	char *v = xstrdup(value);
	xfree(*field);
	*field = v;
}

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

static void label_free(struct label_ctx *l) {
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
	label_free(dc->label);
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

// Sending

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
	if (is_numeric(msg->command) && msg->params.len > 0 &&
	    cstr_eq(irc_msg_param(msg, 0), "*")) {
		irc_msg_set_param(msg, 0, dc->nick);
	}
	const struct cap_registry *caps = &dc->caps;
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

// CAP LS/LIST: with CAP 302, split over several lines if needed, all but
// the last marked with "*"
static void send_cap_list(struct downstream *dc, const char *subcmd, const strvec *caps) {
	AUTOFREE char *host = server_hostname(dc->srv);
	// Room for ":<prefix> CAP <nick> <subcmd> * :" in 512 bytes; a long
	// bouncer-only nick must not underflow
	size_t overhead = 2 + strlen(host) + 2 + strlen(dc->nick) + strlen(subcmd) + 10;
	size_t max = MAX_MESSAGE_LENGTH > overhead ? MAX_MESSAGE_LENGTH - overhead : 0;
	if (max < 64) {
		max = 64;
	}
	strvec lines = {0};
	strvec_push(&lines, "");
	struct strbuf line = STRBUF_INIT;
	for (size_t i = 0; i < caps->len; i++) {
		const char *cap = caps->data[i];
		if (dc->cap_version >= 302 && line.len > 0 && line.len + 1 + strlen(cap) > max) {
			replace_str(&vec_last(&lines), strbuf_cstr(&line));
			strvec_push(&lines, "");
			strbuf_reset(&line);
			strbuf_append_cstr(&line, cap);
		} else {
			if (line.len > 0) {
				strbuf_append_char(&line, ' ');
			}
			strbuf_append_cstr(&line, cap);
		}
	}
	replace_str(&vec_last(&lines), strbuf_cstr(&line));
	strbuf_free(&line);
	for (size_t i = 0; i < lines.len; i++) {
		struct irc_msg *m = IRC_MSG("CAP", dc->nick, subcmd);
		if (i + 1 < lines.len) {
			irc_msg_add_param(m, "*");
		}
		irc_msg_add_param(m, lines.data[i]);
		irc_msg_set_prefix(m, host, "", "");
		downstream_send(dc, m);
	}
	strvec_free(&lines);
}

// Handler results: an IRC error replied to the client, or a fatal error
struct result {
	struct irc_msg *irc;
	char *fatal;
};

static struct result ok(void) {
	return (struct result){0};
}

static struct result irc_err(struct irc_msg *m) {
	return (struct result){.irc = m};
}

static struct result need_more_params(const char *cmd) {
	return irc_err(IRC_MSG(ERR_NEEDMOREPARAMS, "*", cmd, "Not enough parameters"));
}

static bool need_params(const struct irc_msg *m, size_t n) {
	return m->params.len >= n;
}

static void caps_text(struct downstream *dc, bool enabled, strvec *out) {
	const struct strmap *m = enabled ? &dc->caps.enabled : &dc->caps.available;
	const char *k;
	void *v;
	for (size_t it = 0; strmap_next(m, &it, &k, &v);) {
		if (!enabled && dc->cap_version >= 302 && v != nullptr && ((char *)v)[0] != '\0') {
			strvec_push_take(out, xasprintf("%s=%s", k, (char *)v));
		} else {
			strvec_push(out, k);
		}
	}
}

static struct result handle_cap(struct downstream *dc, const struct irc_msg *msg) {
	if (!need_params(msg, 1)) {
		return need_more_params(msg->command);
	}
	AUTO_STRBUF struct strbuf cmd_b = STRBUF_INIT;
	utf8_to_upper(&cmd_b, str_from(msg->params.data[0]));
	const char *cmd = strbuf_cstr(&cmd_b);
	if (cstr_eq(cmd, "LS")) {
		if (msg->params.len > 1) {
			long long v;
			const char *arg = msg->params.data[1];
			if (!str_parse_i64(str_from(arg), &v)) {
				AUTOFREE char *q = utf8_debug_cstr(str_from(arg));
				return (struct result){
					.fatal = xasprintf(
						"strconv.Atoi: parsing %s: invalid syntax", q)};
			}
			dc->cap_version = v;
		}
		if (!dc->registered && dc->cap_version >= 302) {
			// Let downstream show everything it supports, and trim down the
			// available capabilities when upstreams are known
			for (size_t i = 0; i < N(PASSTHROUGH_CAPS); i++) {
				cap_registry_set_available(&dc->caps, PASSTHROUGH_CAPS[i][0],
							   PASSTHROUGH_CAPS[i][1]);
			}
			AUTOFREE char *ml = multiline_limits_cap_value(multiline_limits_default());
			cap_registry_set_available(&dc->caps, MULTILINE_BATCH_TYPE, ml);
		}
		strvec caps = {0};
		caps_text(dc, false, &caps);
		send_cap_list(dc, "LS", &caps);
		strvec_free(&caps);
		if (dc->cap_version >= 302) {
			// CAP version 302 implicitly enables cap-notify
			cap_registry_set_enabled(&dc->caps, "cap-notify", true);
		}
		if (dc->reg != nullptr && !dc->registered) {
			dc->reg->negotiating_caps = true;
		}
	} else if (cstr_eq(cmd, "LIST")) {
		strvec caps = {0};
		caps_text(dc, true, &caps);
		send_cap_list(dc, "LIST", &caps);
		strvec_free(&caps);
	} else if (cstr_eq(cmd, "REQ")) {
		if (msg->params.len < 2) {
			return irc_err(IRC_MSG(ERR_INVALIDCAPCMD, dc->nick, cmd,
					       "Missing argument in CAP REQ command"));
		}
		const char *arg = msg->params.data[1];
		bool ack = true;
		strvec names = {0};
		VEC(bool) enables = {0};
		struct str rest = str_from(arg), word;
		while (ack && utf8_next_word(&rest, &word)) {
			struct strbuf lower = STRBUF_INIT;
			utf8_to_lower(&lower, word);
			struct str name = strbuf_str(&lower);
			bool enable = true;
			if (name.len > 0 && name.p[0] == '-') {
				name = str_slice(name, 1, name.len);
				enable = false;
			}
			AUTOFREE char *n = str_dup(name);
			strbuf_free(&lower);
			if (enable == cap_registry_is_enabled(&dc->caps, n)) {
				continue;
			}
			if (!cap_registry_is_available(&dc->caps, n) ||
			    // cap-notify cannot be disabled with CAP version 302
			    (cstr_eq(n, "cap-notify") && dc->cap_version >= 302 && !enable) ||
			    // account-required is an informational cap
			    cstr_eq(n, "soju.im/account-required")) {
				ack = false;
				break;
			}
			strvec_push(&names, n);
			vec_push(&enables, enable);
		}
		// Atomically ack the whole capability set
		if (ack) {
			for (size_t i = 0; i < names.len; i++) {
				cap_registry_set_enabled(&dc->caps, names.data[i], enables.data[i]);
			}
		}
		strvec_free(&names);
		vec_free(&enables);
		struct irc_msg *m = IRC_MSG("CAP", dc->nick, ack ? "ACK" : "NAK", arg);
		server_set_prefix(dc->srv, m);
		downstream_send(dc, m);
		if (dc->reg != nullptr && !dc->registered) {
			dc->reg->negotiating_caps = true;
		}
	} else if (cstr_eq(cmd, "END")) {
		if (dc->reg != nullptr && !dc->registered) {
			dc->reg->negotiating_caps = false;
		}
	} else {
		return irc_err(IRC_MSG(ERR_INVALIDCAPCMD, dc->nick, cmd, "Unknown CAP command"));
	}
	return ok();
}

void downstream_build_isupport(struct downstream *dc, const struct isupport_ctx *ctx,
			       struct isupport *out) {
	struct server_config *cfg = server_config_get(dc->srv);
	const struct config_basic *b = &cfg->cfg.basic;
	for (size_t i = 0; i < N(PERMANENT_ISUPPORT); i++) {
		isupport_set(out, PERMANENT_ISUPPORT[i], nullptr);
	}
	if (ctx->has_network) {
		char id[24];
		snprintf(id, sizeof(id), "%lld", (long long)ctx->network_id);
		isupport_set(out, "BOUNCER_NETID", id);
	}
	if (!ctx->has_network && b->title[0] != '\0') {
		isupport_set(out, "NETWORK", b->title);
	}
	if (!ctx->has_network && cfg->icon_url[0] != '\0') {
		isupport_set(out, "draft/ICON", cfg->icon_url);
	} else if (!ctx->has_network && cfg->icon_path[0] != '\0') {
		AUTOFREE char *icon = xasprintf("%s/icon", b->http_ingress);
		isupport_set(out, "draft/ICON", icon);
	}
	if (ctx->has_user) {
		if (!ctx->has_network) {
			isupport_set(out, "BOT", "B");
			isupport_set(out, "CASEMAPPING", "ascii");
			isupport_set(out, "WHOX", nullptr);
			// channels are not supported
			isupport_set(out, "CHANTYPES", "");
			// default bufio.Reader size
			isupport_set(out, "LINELEN", "4096");
		}
		if (ctx->chathistory && ctx->has_network) {
			char limit[24];
			snprintf(limit, sizeof(limit), "%zu", b->chat_history_limit);
			isupport_set(out, "CHATHISTORY", limit);
			isupport_set(out, "MSGREFTYPES", "timestamp");
		}
		if (cap_registry_is_enabled(&dc->caps, "soju.im/webpush") &&
		    ctx->vapid_public != nullptr) {
			isupport_set(out, "VAPID", ctx->vapid_public);
		}
	}
	if (ctx->up_isupport != nullptr) {
		// If upstream doesn't support message-tags, indicate that we'll drop
		// all of them
		if (!strmap_has(&ctx->up_isupport->m, "CLIENTTAGDENY") &&
		    (ctx->up_caps == nullptr ||
		     !cap_registry_is_enabled(ctx->up_caps, "message-tags"))) {
			isupport_set(out, "CLIENTTAGDENY", "*");
		}
		for (size_t i = 0; i < N(PASSTHROUGH_ISUPPORT); i++) {
			const char *k = PASSTHROUGH_ISUPPORT[i];
			if (strmap_has(&ctx->up_isupport->m, k)) {
				isupport_set(out, k, strmap_get(&ctx->up_isupport->m, k));
			}
		}
	}
	server_config_unref(cfg);
}

void downstream_send_isupport(struct downstream *dc, const struct isupport *is) {
	irc_msg_vec msgs = {0};
	generate_isupport(&msgs, is);
	if (cap_registry_is_enabled(&dc->caps, "draft/extended-isupport")) {
		downstream_send_batch(dc, "draft/isupport", nullptr, 0, &msgs);
	} else {
		for (size_t i = 0; i < msgs.len; i++) {
			downstream_send(dc, msgs.data[i]);
		}
		vec_free(&msgs);
	}
}

// Utilities

void unmarshal_username(const char *raw, char **username, char **client, char **network) {
	struct str s = str_from(raw);
	ptrdiff_t i = -1, j = -1;
	for (size_t k = 0; k < s.len; k++) {
		if (s.p[k] == '/' || s.p[k] == '@') {
			if (i < 0) {
				i = (ptrdiff_t)k;
			}
			j = (ptrdiff_t)k;
		}
	}
	*username = str_dup(i >= 0 ? str_slice(s, 0, (size_t)i) : s);
	*client = xstrdup("");
	*network = xstrdup("");
	if (j >= 0) {
		struct str after = str_slice(s, (size_t)j + 1, s.len);
		char *v = str_dup(after);
		char **dst = s.p[j] == '@' ? client : network;
		xfree(*dst);
		*dst = v;
	}
	if (i >= 0 && j >= 0 && i < j) {
		char *v = str_dup(str_slice(s, (size_t)i + 1, (size_t)j));
		char **dst = s.p[i] == '@' ? client : network;
		xfree(*dst);
		*dst = v;
	}
}

bool parse_nickserv_credentials(const char *text, const char *nick, char **account,
				char **password) {
	strvec fields = {0};
	struct str rest = str_from(text), w;
	while (utf8_next_word(&rest, &w)) {
		strvec_push_take(&fields, str_dup(w));
	}
	bool ok = false;
	if (fields.len >= 2) {
		AUTO_STRBUF struct strbuf cmd = STRBUF_INIT;
		utf8_to_upper(&cmd, str_from(fields.data[0]));
		const char *c = strbuf_cstr(&cmd);
		size_t np = fields.len - 1;
		char **p = fields.data + 1;
		if (cstr_eq(c, "REGISTER")) {
			*account = xstrdup(nick);
			*password = xstrdup(p[0]);
			ok = true;
		} else if (cstr_eq(c, "IDENTIFY")) {
			*account = xstrdup(np == 1 ? nick : p[0]);
			*password = xstrdup(np == 1 ? p[0] : p[1]);
			ok = true;
		} else if (cstr_eq(c, "SET") && np == 2 &&
			   str_eq_ignore_ascii_case(str_from(p[0]), STR_LIT("PASSWORD"))) {
			// Upstream also reports success for other SET subcommands, with
			// empty credentials which then overwrite the saved ones
			*account = xstrdup(nick);
			*password = xstrdup(p[1]);
			ok = true;
		}
	}
	strvec_free(&fields);
	return ok;
}

void fill_network_addr_attrs(struct irc_tags *attrs, const struct db_network *n) {
	struct url u;
	if (!network_url(n->addr, &u, nullptr)) {
		return;
	}
	bool has_host_port = false;
	if (cstr_eq(u.scheme, "ircs")) {
		irc_tags_set(attrs, "tls", "1");
		has_host_port = true;
	} else if (cstr_eq(u.scheme, "irc+insecure")) {
		irc_tags_set(attrs, "tls", "0");
		has_host_port = true;
	}
	if (has_host_port) {
		char *host, *port;
		if (split_host_port(str_from(u.host), &host, &port, nullptr)) {
			irc_tags_set(attrs, "host", host);
			irc_tags_set(attrs, "port", port);
			xfree(host);
			xfree(port);
		} else {
			irc_tags_set(attrs, "host", u.host);
		}
	}
	url_free(&u);
}

static char *network_addr_from_attrs(const struct irc_tags *attrs) {
	const char *host = irc_tags_get_or_empty(attrs, "host");
	if (host[0] == '\0') {
		return xstrdup("");
	}
	struct strbuf addr = STRBUF_INIT;
	strbuf_append_cstr(&addr, host);
	const char *port = irc_tags_get_or_empty(attrs, "port");
	if (port[0] != '\0') {
		strbuf_printf(&addr, ":%s", port);
	}
	if (cstr_eq(irc_tags_get_or_empty(attrs, "tls"), "0")) {
		AUTOFREE char *a = strbuf_take(&addr);
		return xasprintf("irc+insecure://%s", a);
	}
	return strbuf_take(&addr);
}

bool update_network_attrs(struct db_network *n, const struct irc_tags *attrs,
			  const char *subcommand, struct irc_msg **irc) {
	struct irc_tags addr_attrs = IRC_TAGS_INIT;
	fill_network_addr_attrs(&addr_attrs, n);
	bool update_addr = false;
	bool ok = true;
	for (size_t i = 0; ok && i < attrs->v.len; i++) {
		const char *k = attrs->v.data[i].key, *v = attrs->v.data[i].value;
		if (cstr_eq(k, "host") || cstr_eq(k, "port") || cstr_eq(k, "tls")) {
			update_addr = true;
			irc_tags_set(&addr_attrs, k, v);
		} else if (cstr_eq(k, "name")) {
			replace_str(&n->name, v);
		} else if (cstr_eq(k, "nickname")) {
			replace_str(&n->nick, v);
		} else if (cstr_eq(k, "username")) {
			replace_str(&n->username, v);
		} else if (cstr_eq(k, "realname")) {
			replace_str(&n->realname, v);
		} else if (cstr_eq(k, "pass")) {
			replace_str(&n->pass, v);
		} else {
			*irc = IRC_MSG("FAIL", "BOUNCER", "UNKNOWN_ATTRIBUTE", subcommand, k,
				       "Unknown attribute");
			ok = false;
		}
	}
	if (ok && update_addr) {
		char *addr = network_addr_from_attrs(&addr_attrs);
		xfree(n->addr);
		n->addr = addr;
		if (addr[0] == '\0') {
			*irc = IRC_MSG("FAIL", "BOUNCER", "NEED_ATTRIBUTE", subcommand, "host",
				       "Missing required host attribute");
			ok = false;
		}
	}
	irc_tags_free(&addr_attrs);
	return ok;
}

// Registration

static void begin_message(struct downstream *dc, const struct irc_msg *msg) {
	label_free(dc->label);
	dc->label = nullptr;
	const char *label = irc_tags_get(&msg->tags, "label");
	if (label != nullptr && cap_registry_is_enabled(&dc->caps, "labeled-response")) {
		dc->label = xcalloc(1, sizeof(*dc->label));
		dc->label->label = xstrdup(label);
		dc->label->batch = xstrdup("");
	}
}

void downstream_begin_command(struct downstream *dc, const struct irc_msg *msg) {
	begin_message(dc, msg);
}

// Sends an IRC error to the client; returns the fatal error, if any
static char *finish_message(struct downstream *dc, struct result res) {
	if (res.fatal != nullptr) {
		label_free(dc->label);
		dc->label = nullptr;
		irc_msg_free(res.irc);
		return res.fatal;
	}
	if (res.irc != nullptr) {
		server_set_prefix(dc->srv, res.irc);
		downstream_send(dc, res.irc);
	}
	downstream_flush_batch(dc);
	label_free(dc->label);
	dc->label = nullptr;
	return nullptr;
}

static void set_auth_username(struct downstream *dc, const char *username, const char *client,
			      const char *network) {
	replace_str(&dc->client_name, client);
	replace_str(&dc->reg->auth_username, username);
	replace_str(&dc->reg->network_name, network);
}

static void fail_connection(struct downstream *dc, const char *log_msg) {
	if (log_msg != nullptr) {
		log_info(&dc->log, "%s", log_msg);
	}
	downstream_close(dc);
}

static void try_register(struct downstream *dc);
static void resume(struct downstream *dc);

// Asynchronous SASL: the completion looks the downstream up by ID
struct sasl_job {
	uint64_t dc_id;
	struct server *srv;
	char *mechanism;
	char *nick;
	// PLAIN
	char *username, *client, *network, *identity_username, *identity_client, *identity_network;
	bool impersonating;
	// EXTERNAL
	struct strbuf fingerprint;
	struct db_device_certificate cert;
	int64_t user_id;
	char *ip;
	// results
	char *err;
	bool found;
	struct db_user user;
};

static void sasl_job_free(struct sasl_job *j) {
	xfree(j->mechanism);
	xfree(j->nick);
	xfree(j->username);
	xfree(j->client);
	xfree(j->network);
	xfree(j->identity_username);
	xfree(j->identity_client);
	xfree(j->identity_network);
	strbuf_free(&j->fingerprint);
	db_device_certificate_free(&j->cert);
	xfree(j->ip);
	xfree(j->err);
	if (j->found) {
		db_user_free(&j->user);
	}
	xfree(j);
}

// Ends an asynchronous SASL exchange: err nullptr on success
static void sasl_finish(struct sasl_job *j, const char *internal, const char *external) {
	struct downstream *dc = downstream_lookup(j->dc_id);
	if (dc == nullptr) {
		sasl_job_free(j);
		return;
	}
	if (internal != nullptr) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(dc->reg->nick));
		log_info(&dc->log, "SASL %s authentication error for nick %s: %s", j->mechanism, q,
			 internal);
		downstream_end_sasl(
			dc, IRC_MSG(ERR_SASLFAIL, dc->nick,
				    external != nullptr ? external : "Authentication failed"));
	} else {
		bool impersonating = j->impersonating;
		set_auth_username(dc, impersonating ? j->identity_username : j->username,
				  impersonating ? j->identity_client : j->client,
				  impersonating ? j->identity_network : j->network);
		dc->impersonating = impersonating;
		// Technically we should send RPL_LOGGEDIN here. However we use
		// RPL_LOGGEDIN to mirror the upstream connection status. See:
		// https://github.com/ircv3/ircv3-specifications/pull/476
		downstream_end_sasl(dc, nullptr);
	}
	sasl_job_free(j);
	dc->busy = false;
	resume(dc);
}

static void run_get_admin(struct db *db, void *data) {
	struct sasl_job *j = data;
	struct err *e = nullptr;
	bool found;
	struct db_user u;
	if (!db_get_user(db, j->username, &u, &found, &e)) {
		j->err = xstrdup(err_msg(e));
		err_free(e);
		return;
	}
	if (!found) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(j->username));
		j->err = xasprintf("sql: no rows in result set (username %s)", q);
		return;
	}
	bool admin = u.admin;
	db_user_free(&u);
	if (!admin) {
		j->err = xstrdup("SASL impersonation only allowed for admin users");
		return;
	}
	if (!db_get_user(db, j->identity_username, &u, &found, &e)) {
		j->err = xstrdup(err_msg(e));
		err_free(e);
		return;
	}
	if (!found) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(j->identity_username));
		j->err = xasprintf("sql: no rows in result set (username %s)", q);
		return;
	}
	db_user_free(&u);
}

static void done_get_admin(void *data) {
	struct sasl_job *j = data;
	sasl_finish(j, j->err, nullptr);
}

static void plain_auth_done(void *data, const struct auth_error *err) {
	struct sasl_job *j = data;
	if (err != nullptr) {
		// Like upstream (which formats the error with %v), this drops the
		// user-facing message: the client gets "Authentication failed"
		AUTOFREE char *q = utf8_debug_cstr(str_from(j->username));
		AUTOFREE char *internal = xasprintf("%s (username %s)", err->internal, q);
		sasl_finish(j, internal, nullptr);
		return;
	}
	if (j->impersonating) {
		server_db(j->srv, run_get_admin, done_get_admin, j);
		return;
	}
	sasl_finish(j, nullptr, nullptr);
}

static void run_external(struct db *db, void *data) {
	struct sasl_job *j = data;
	struct err *e = nullptr;
	bool found;
	if (!db_get_device_certificate(db, strbuf_str(&j->fingerprint), &j->user_id, &j->cert,
				       &found, &e)) {
		j->err = xstrdup(err_msg(e));
		err_free(e);
		return;
	}
	if (!found) {
		j->err = xstrdup("Unknown TLS client certificate");
		return;
	}
	char *name = nullptr;
	if (!db_get_username_by_id(db, j->user_id, &name, &e)) {
		j->err = xstrdup(err_msg(e));
		err_free(e);
		return;
	}
	if (name == nullptr) {
		j->err = xstrdup("sql: no rows in result set");
		return;
	}
	j->username = name;
	db_device_certificate_mark_used(&j->cert, j->ip);
	if (!db_store_device_certificate(db, j->user_id, &j->cert, &e)) {
		j->err = xstrdup(err_msg(e));
		err_free(e);
	}
}

static void done_external(void *data) {
	struct sasl_job *j = data;
	sasl_finish(j, j->err, nullptr);
}

// Starts checking SASL credentials; dc is busy until sasl_finish
static void authenticate(struct downstream *dc, const struct downstream_sasl *s) {
	struct sasl_job *j = xcalloc(1, sizeof(*j));
	j->dc_id = dc->id;
	j->srv = dc->srv;
	j->mechanism = xstrdup(s->mechanism);
	j->client = xstrdup("");
	j->network = xstrdup("");
	struct server_config *cfg = server_config_get(dc->srv);
	enum plain_auth plain = cfg->plain_auth;
	bool oauth = cfg->oauth_bearer;
	bool client_cert_auth = cfg->cfg.basic.client_cert_auth;
	server_config_unref(cfg);
	dc->busy = true;
	if (s->kind == SASL_PLAIN) {
		xfree(j->client);
		xfree(j->network);
		unmarshal_username(s->plain.username, &j->username, &j->client, &j->network);
		if (plain == PLAIN_AUTH_NONE) {
			sasl_finish(j, "SASL PLAIN not supported", nullptr);
			return;
		}
		if (s->plain.identity[0] != '\0' &&
		    !cstr_eq(s->plain.identity, s->plain.username)) {
			j->impersonating = true;
			unmarshal_username(s->plain.identity, &j->identity_username,
					   &j->identity_client, &j->identity_network);
		}
		auth_plain(dc->srv, j->username, s->plain.password, dc->remote_addr,
			   plain_auth_done, j);
		return;
	}
	if (s->kind == SASL_OAUTHBEARER) {
		sasl_finish(j,
			    oauth ? "OAuth 2.0 isn't supported yet"
				  : "SASL OAUTHBEARER not supported",
			    nullptr);
		return;
	}
	if (s->kind == SASL_EXTERNAL) {
		if (!client_cert_auth) {
			sasl_finish(j, "SASL EXTERNAL not supported", nullptr);
			return;
		}
		if (!dc->has_peer_cert) {
			sasl_finish(j, "TLS client certificate not sent", nullptr);
			return;
		}
		unsigned char md[EVP_MAX_MD_SIZE];
		unsigned len = 0;
		EVP_Digest(dc->peer_cert.buf, dc->peer_cert.len, md, &len, EVP_sha512(), nullptr);
		strbuf_append(&j->fingerprint, (const char *)md, len);
		db_device_certificate_init(&j->cert);
		if (dc->has_remote_ip) {
			struct strbuf ip = STRBUF_INIT;
			ipaddr_format(&ip, dc->remote_ip);
			j->ip = strbuf_take(&ip);
		}
		server_db(dc->srv, run_external, done_external, j);
		return;
	}
	sasl_finish(j, "unsupported SASL mechanism", nullptr);
}

// AUTHENTICATE: returns true with *complete set once the client has sent
// its credentials
static struct result handle_authenticate(struct downstream *dc, const struct irc_msg *msg,
					 bool *complete) {
	*complete = false;
	if (!cap_registry_is_enabled(&dc->caps, "sasl")) {
		return irc_err(
			IRC_MSG(ERR_SASLFAIL, dc->nick,
				"AUTHENTICATE requires the \"sasl\" capability to be enabled"));
	}
	if (msg->params.len == 0) {
		return irc_err(IRC_MSG(ERR_SASLFAIL, dc->nick, "Missing AUTHENTICATE argument"));
	}
	const char *param = msg->params.data[0];
	if (cstr_eq(param, "*")) {
		return irc_err(IRC_MSG(ERR_SASLABORTED, dc->nick, "SASL authentication aborted"));
	}
	struct strbuf decoded = STRBUF_INIT;
	bool has_resp = false;
	if (dc->sasl == nullptr) {
		AUTO_STRBUF struct strbuf mech = STRBUF_INIT;
		utf8_to_upper(&mech, str_from(param));
		enum sasl_kind kind;
		const char *m = strbuf_cstr(&mech);
		if (cstr_eq(m, "PLAIN")) {
			kind = SASL_PLAIN;
		} else if (cstr_eq(m, "OAUTHBEARER")) {
			kind = SASL_OAUTHBEARER;
		} else if (cstr_eq(m, "ANONYMOUS")) {
			kind = SASL_ANONYMOUS;
		} else if (cstr_eq(m, "EXTERNAL")) {
			kind = SASL_EXTERNAL;
		} else {
			return irc_err(
				IRC_MSG(ERR_SASLFAIL, dc->nick, "Unsupported SASL mechanism"));
		}
		dc->sasl = xcalloc(1, sizeof(*dc->sasl));
		dc->sasl->kind = kind;
		dc->sasl->mechanism = xstrdup(m);
	} else {
		struct downstream_sasl *s = dc->sasl;
		const char *chunk = cstr_eq(param, "+") ? "" : param;
		if (s->pending_resp.len + strlen(chunk) > (size_t)10 * 1024) {
			return irc_err(IRC_MSG(ERR_SASLFAIL, dc->nick, "Response too long"));
		}
		strbuf_append_cstr(&s->pending_resp, chunk);
		if (strlen(chunk) == MAX_SASL_LENGTH) {
			return ok(); // Multi-line response, wait for the next command
		}
		if (!base64_decode(&decoded, BASE64_STANDARD, strbuf_str(&s->pending_resp),
				   nullptr)) {
			strbuf_free(&decoded);
			return irc_err(
				IRC_MSG(ERR_SASLFAIL, dc->nick, "Invalid base64-encoded response"));
		}
		strbuf_reset(&s->pending_resp);
		has_resp = true;
	}
	struct str resp = strbuf_str(&decoded);
	struct strbuf *challenge;
	struct err *e = nullptr;
	bool ok_next = downstream_sasl_next(dc->sasl, has_resp ? &resp : nullptr, &challenge, &e);
	strbuf_free(&decoded);
	if (!ok_next) {
		struct result r = {.fatal = xstrdup(err_msg(e))};
		err_free(e);
		return r;
	}
	if (challenge == nullptr) {
		*complete = true;
		return ok();
	}
	AUTOFREE char *s = challenge->len == 0 ? xstrdup("+")
					       : base64_encode_cstr(BASE64_STANDARD, challenge->buf,
								    challenge->len);
	strbuf_free(challenge);
	xfree(challenge);
	// TODO (upstream): multi-line messages
	downstream_send(dc, IRC_MSG("AUTHENTICATE", s));
	return ok();
}

// METADATA SUB/UNSUB/SUBS (soju-rs sync.rs handle_metadata_sub), before
// registration: no message targets to send. Returns false for other
// subcommands.
static bool handle_metadata_sub(struct downstream *dc, const struct irc_msg *msg) {
	const char *sub = msg->params.data[1];
	bool subscribe = cstr_eq(sub, "SUB");
	if (subscribe || cstr_eq(sub, "UNSUB")) {
		for (size_t i = 2; i < msg->params.len; i++) {
			struct strbuf lower = STRBUF_INIT;
			utf8_to_lower(&lower, str_from(msg->params.data[i]));
			const char *k = strbuf_cstr(&lower);
			if (!cstr_eq(k, "soju.im/pinned") && !cstr_eq(k, "soju.im/muted") &&
			    !cstr_eq(k, "soju.im/blocked")) {
				downstream_send(dc, IRC_MSG("FAIL", msg->command, "KEY_INVALID", k,
							    "Invalid key"));
			} else if (subscribe) {
				strmap_set(&dc->metadata_subs, k, nullptr);
				downstream_send(dc, IRC_MSG(RPL_METADATASUBOK, "*", k));
			} else {
				strmap_remove(&dc->metadata_subs, k);
				downstream_send(dc, IRC_MSG(RPL_METADATAUNSUBOK, "*", k));
			}
			strbuf_free(&lower);
		}
		return true;
	}
	if (cstr_eq(sub, "SUBS")) {
		const char *k;
		for (size_t it = 0; strmap_next(&dc->metadata_subs, &it, &k, nullptr);) {
			downstream_send(dc, IRC_MSG(RPL_METADATASUBS, "*", k));
		}
		return true;
	}
	return false;
}

static struct result handle_unregistered(struct downstream *dc, const struct irc_msg *msg) {
	const char *cmd = msg->command;
	struct registration *r = dc->reg;
	if (cstr_eq(cmd, "NICK")) {
		if (!need_params(msg, 1)) {
			return need_more_params(cmd);
		}
		replace_str(&r->nick, msg->params.data[0]);
	} else if (cstr_eq(cmd, "USER")) {
		if (!need_params(msg, 4)) {
			return need_more_params(cmd);
		}
		replace_str(&r->username, msg->params.data[0]);
	} else if (cstr_eq(cmd, "PASS")) {
		if (!need_params(msg, 1)) {
			return need_more_params(cmd);
		}
		replace_str(&r->pass, msg->params.data[0]);
	} else if (cstr_eq(cmd, "CAP")) {
		return handle_cap(dc, msg);
	} else if (cstr_eq(cmd, "AUTHENTICATE")) {
		bool complete;
		struct result res = handle_authenticate(dc, msg, &complete);
		if (res.irc != nullptr || res.fatal != nullptr) {
			downstream_sasl_free(dc->sasl);
			dc->sasl = nullptr;
			return res;
		}
		if (complete) {
			authenticate(dc, dc->sasl);
		}
	} else if (cstr_eq(cmd, "BOUNCER")) {
		if (!need_params(msg, 1)) {
			return need_more_params(cmd);
		}
		const char *sub = msg->params.data[0];
		AUTO_STRBUF struct strbuf up = STRBUF_INIT;
		utf8_to_upper(&up, str_from(sub));
		if (!cstr_eq(strbuf_cstr(&up), "BIND")) {
			return irc_err(IRC_MSG("FAIL", "BOUNCER", "UNKNOWN_COMMAND", sub,
					       "Unknown subcommand"));
		}
		if (!need_params(msg, 2)) {
			return need_more_params(cmd);
		}
		if (r->auth_username[0] == '\0') {
			return irc_err(IRC_MSG("FAIL", "BOUNCER", "ACCOUNT_REQUIRED", "BIND",
					       "Authentication needed to bind to bouncer network"));
		}
		long long id;
		const char *s = msg->params.data[1];
		if (!str_parse_i64(str_from(s), &id) || id <= 0) {
			return irc_err(IRC_MSG("FAIL", "BOUNCER", "INVALID_NETID", sub, s,
					       "Invalid network ID"));
		}
		r->network_id = id;
	} else if (cstr_eq(cmd, "AWAY")) {
		xfree(dc->away);
		dc->away = msg->params.len > 0 ? xstrdup(msg->params.data[0]) : nullptr;
		downstream_send(dc, generate_away_reply(dc->away != nullptr));
	} else if (cstr_eq(cmd, "METADATA")) {
		if (!need_params(msg, 2)) {
			return need_more_params(cmd);
		}
		const char *sub = msg->params.data[1];
		if (!handle_metadata_sub(dc, msg)) {
			return irc_err(IRC_MSG("FAIL", "METADATA", "INVALID_PARAMS", sub,
					       "Unknown command"));
		}
	} else {
		log_debug(&dc->log, "unhandled message: %s", cmd);
		return irc_err(IRC_MSG(ERR_UNKNOWNCOMMAND, "*", cmd, "Unknown command"));
	}
	return ok();
}

// After PASS: authenticated asynchronously
struct pass_job {
	uint64_t dc_id;
	char *reg_username;
	char *username, *client, *network;
};

static void pass_job_free(struct pass_job *j) {
	xfree(j->reg_username);
	xfree(j->username);
	xfree(j->client);
	xfree(j->network);
	xfree(j);
}

static void reply_and_close(struct downstream *dc, struct irc_msg *m) {
	server_set_prefix(dc->srv, m);
	downstream_send(dc, m);
	downstream_close(dc);
}

static void finish_registration(struct downstream *dc);

static void pass_done(void *data, const struct auth_error *err) {
	struct pass_job *j = data;
	struct downstream *dc = downstream_lookup(j->dc_id);
	if (dc == nullptr) {
		pass_job_free(j);
		return;
	}
	dc->busy = false;
	if (err != nullptr) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(j->reg_username));
		log_info(&dc->log, "PASS authentication error for user %s: %s", q, err->internal);
		reply_and_close(dc, IRC_MSG(ERR_PASSWDMISMATCH, dc->nick, err->external));
		pass_job_free(j);
		return;
	}
	set_auth_username(dc, j->username, j->client, j->network);
	pass_job_free(j);
	finish_registration(dc);
	drain_input(dc);
}

// soju-rs register(): authentication by PASS if SASL wasn't used, then
// the client and network names
static void try_register(struct downstream *dc) {
	if (dc->sasl != nullptr) {
		downstream_end_sasl(
			dc, IRC_MSG(ERR_SASLABORTED, dc->nick, "SASL authentication aborted"));
	}
	struct registration *r = dc->reg;
	char *password = r->pass;
	r->pass = xstrdup("");
	if (r->auth_username[0] != '\0') {
		explicit_bzero(password, strlen(password));
		xfree(password);
		finish_registration(dc);
		return;
	}
	if (password[0] == '\0') {
		xfree(password);
		if (cap_registry_is_enabled(&dc->caps, "sasl")) {
			reply_and_close(dc, IRC_MSG("FAIL", "*", "ACCOUNT_REQUIRED",
						    "Authentication required"));
		} else {
			reply_and_close(dc, IRC_MSG(ERR_PASSWDMISMATCH, dc->nick,
						    "Authentication required"));
		}
		return;
	}
	struct server_config *cfg = server_config_get(dc->srv);
	bool plain = cfg->plain_auth != PLAIN_AUTH_NONE;
	server_config_unref(cfg);
	if (!plain) {
		explicit_bzero(password, strlen(password));
		xfree(password);
		reply_and_close(
			dc, IRC_MSG(ERR_PASSWDMISMATCH, dc->nick, "PASS authentication disabled"));
		return;
	}
	struct pass_job *j = xcalloc(1, sizeof(*j));
	j->dc_id = dc->id;
	j->reg_username = xstrdup(r->username);
	unmarshal_username(r->username, &j->username, &j->client, &j->network);
	dc->busy = true;
	auth_plain(dc->srv, j->username, password, dc->remote_addr, pass_done, j);
	explicit_bzero(password, strlen(password));
	xfree(password);
}

static void finish_registration(struct downstream *dc) {
	struct registration *r = dc->reg;
	AUTOFREE char *u = nullptr;
	AUTOFREE char *fallback_client = nullptr;
	AUTOFREE char *fallback_network = nullptr;
	unmarshal_username(r->username, &u, &fallback_client, &fallback_network);
	if (dc->client_name[0] == '\0') {
		replace_str(&dc->client_name, fallback_client);
	} else if (fallback_client[0] != '\0' && !cstr_eq(dc->client_name, fallback_client)) {
		reply_and_close(dc, IRC_MSG(ERR_ERRONEUSNICKNAME, dc->nick,
					    "Client name mismatch in usernames"));
		return;
	}
	if (r->network_name[0] == '\0') {
		replace_str(&r->network_name, fallback_network);
	} else if (fallback_network[0] != '\0' && !cstr_eq(r->network_name, fallback_network)) {
		reply_and_close(dc, IRC_MSG(ERR_ERRONEUSNICKNAME, dc->nick,
					    "Network name mismatch in usernames"));
		return;
	}
	dc->registered = true;
	replace_str(&dc->username, r->auth_username);
	AUTOFREE char *q = utf8_debug_cstr(str_from(dc->username));
	log_info(&dc->log, "registration complete for user %s", q);
	uv_timer_stop(&dc->register_timer);
	if (dc->events.registered != nullptr) {
		dc->events.registered(dc, dc->events.data);
	} else {
		downstream_close(dc);
	}
}

// Checks whether registration can complete, after a message
static void maybe_register(struct downstream *dc) {
	if (dc->closed || dc->registered || dc->busy) {
		return;
	}
	struct registration *r = dc->reg;
	if (r->nick[0] == '\0' || r->username[0] == '\0' || r->negotiating_caps) {
		return;
	}
	try_register(dc);
}

static void handle_message_unregistered(struct downstream *dc, struct irc_msg *msg) {
	atomic_fetch_add(&dc->srv->metrics.downstream_in_messages_total, 1);
	begin_message(dc, msg);
	struct result res;
	if (cstr_eq(msg->command, "QUIT")) {
		conn_shutdown(dc->conn);
		res = ok();
	} else if (cstr_eq(msg->command, "ISUPPORT")) {
		if (!cap_registry_is_enabled(&dc->caps, "draft/extended-isupport")) {
			res = irc_err(
				IRC_MSG(ERR_UNKNOWNCOMMAND, "*", msg->command, "Unknown command"));
		} else {
			struct isupport_ctx ctx = {0};
			struct isupport is;
			isupport_init(&is);
			downstream_build_isupport(dc, &ctx, &is);
			downstream_send_isupport(dc, &is);
			isupport_free(&dc->isupport);
			dc->isupport = is;
			res = ok();
		}
	} else {
		res = handle_unregistered(dc, msg);
	}
	AUTOFREE char *fatal = finish_message(dc, res);
	if (fatal != nullptr) {
		AUTOFREE char *redacted = redact_for_log(msg);
		AUTOFREE char *q = utf8_debug_cstr(str_from(redacted));
		AUTOFREE char *line = xasprintf("failed to handle IRC command %s: %s", q, fatal);
		fail_connection(dc, line);
	}
}

// Pending input while busy: processed in order once the async step ends
static void process_input(struct conn *c, struct irc_msg *msg, void *data) {
	struct downstream *dc = data;
	if (dc->registered && !dc->closed) {
		downstream_receive_take(dc, msg);
		return;
	}
	if (dc->closed) {
		irc_msg_free(msg);
		conn_input_done(dc->conn);
		return;
	}
	handle_message_unregistered(dc, msg);
	irc_msg_free(msg);
	// dc may be gone (closed and freed after the loop iteration): it's
	// still allocated until its timer closes
	if (dc->closed) {
		conn_input_done(c);
		return;
	}
	if (dc->busy) {
		// Holds this message's input slot until the step completes: later
		// messages wait, like the sequential soju-rs loop
		dc->waiting_input = true;
		return;
	}
	conn_input_done(c);
	maybe_register(dc);
}

static void drain_input(struct downstream *dc) {
	if (dc->processing_input || dc->closed) {
		return;
	}
	dc->processing_input = true;
	while (!dc->busy && !dc->closed && dc->pending_input.len > 0) {
		struct irc_msg *msg = dc->pending_input.data[0];
		vec_remove(&dc->pending_input, 0);
		process_input(dc->conn, msg, dc);
	}
	dc->processing_input = false;
}

static void on_message(struct conn *c, struct irc_msg *msg, void *data) {
	(void)c;
	struct downstream *dc = data;
	vec_push(&dc->pending_input, msg);
	drain_input(dc);
}

void downstream_input_done(struct downstream *dc) {
	if (dc->conn != nullptr) {
		conn_input_done(dc->conn);
	}
}

void downstream_receive_take(struct downstream *dc, struct irc_msg *msg) {
	if (dc->closed) {
		irc_msg_free(msg);
		downstream_input_done(dc);
		return;
	}
	const char *tag = irc_tags_get(&msg->tags, "soju.im/network");
	if (!dc->has_parent && tag != nullptr &&
	    cap_registry_is_enabled(&dc->caps, "soju.im/multiplex")) {
		long long id;
		struct downstream *child = str_parse_i64(str_from(tag), &id)
						   ? downstream_find_attached(dc->id, id)
						   : nullptr;
		if (child == nullptr) {
			if (!cstr_eq(msg->command, "PING") && !cstr_eq(msg->command, "PONG")) {
				downstream_begin_command(dc, msg);
				downstream_send(dc, IRC_MSG("FAIL", msg->command,
							    "NETWORK_NOT_ATTACHED", tag,
							    "Network not attached"));
				downstream_end_command(dc);
			}
			irc_msg_free(msg);
			downstream_input_done(dc);
			return;
		}
		irc_tags_remove(&msg->tags, "soju.im/network");
		dc = child;
	}
	atomic_fetch_add(&dc->srv->metrics.downstream_in_messages_total, 1);
	if (cstr_eq(msg->command, "CAP")) {
		begin_message(dc, msg);
		AUTOFREE char *fatal = finish_message(dc, handle_cap(dc, msg));
		if (fatal != nullptr) {
			fail_connection(dc, fatal);
		}
		if (!dc->closed && dc->events.updated != nullptr) {
			dc->events.updated(dc, dc->events.data);
		}
	} else if (cstr_eq(msg->command, "QUIT")) {
		downstream_close(dc);
	} else if (dc->events.message != nullptr) {
		dc->events.message(dc, msg, dc->events.data);
		return;
	} else {
		begin_message(dc, msg);
		downstream_send(
			dc, IRC_MSG(ERR_UNKNOWNCOMMAND, dc->nick, msg->command, "Unknown command"));
		downstream_end_command(dc);
	}
	irc_msg_free(msg);
	downstream_input_done(dc);
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

static void resume(struct downstream *dc) {
	if (dc->waiting_input) {
		dc->waiting_input = false;
		conn_input_done(dc->conn);
	}
	if (dc->conn != nullptr && conn_is_closed(dc->conn)) {
		downstream_close(dc);
		return;
	}
	maybe_register(dc);
	drain_input(dc);
}

void downstream_resume_input(struct downstream *dc) {
	dc->busy = false;
	resume(dc);
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
	reply_and_close(dc, m);
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
	if (cfg->oauth_bearer) {
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
	conn_set_callbacks(c, &(struct conn_callbacks){.message = on_message, .closed = on_closed},
			   dc);
	if (atomic_load(&srv->shutting_down)) {
		downstream_send(dc, IRC_MSG("ERROR", "Server is shutting down"));
		downstream_close(dc);
		return;
	}
	uv_timer_start(&dc->register_timer, on_register_timeout, DOWNSTREAM_REGISTER_TIMEOUT_MS, 0);
}
