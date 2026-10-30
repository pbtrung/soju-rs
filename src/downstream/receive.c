// Receiving from downstreams: per-message setup, unregistered dispatch
// and the pending input of a busy connection (soju-rs downstream/mod.rs)
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

static void begin_message(struct downstream *dc, const struct irc_msg *msg) {
	downstream_label_free(dc->label);
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
		downstream_label_free(dc->label);
		dc->label = nullptr;
		irc_msg_free(res.irc);
		return res.fatal;
	}
	if (res.irc != nullptr) {
		server_set_prefix(dc->srv, res.irc);
		downstream_send(dc, res.irc);
	}
	downstream_flush_batch(dc);
	downstream_label_free(dc->label);
	dc->label = nullptr;
	return nullptr;
}

void downstream_set_auth_username(struct downstream *dc, const char *username, const char *client,
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

// Checks whether registration can complete, after a message
void downstream_maybe_register(struct downstream *dc) {
	if (dc->closed || dc->registered || dc->busy) {
		return;
	}
	struct registration *r = dc->reg;
	if (r->nick[0] == '\0' || r->username[0] == '\0' || r->negotiating_caps) {
		return;
	}
	downstream_try_register(dc);
}

// Output waiting to be sent above which an unregistered connection is
// closed: a peer sending commands without reading their replies (e.g.
// CAP LS, a few bytes for hundreds) would otherwise make the server hold
// up to MAX_QUEUED_BYTES per connection before even authenticating
static constexpr size_t MAX_UNREGISTERED_QUEUED_BYTES = (size_t)256 << 10U;

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
		res = downstream_handle_unregistered(dc, msg);
	}
	AUTOFREE char *fatal = finish_message(dc, res);
	if (fatal != nullptr) {
		AUTOFREE char *redacted = redact_for_log(msg);
		AUTOFREE char *q = utf8_debug_cstr(str_from(redacted));
		AUTOFREE char *line = xasprintf("failed to handle IRC command %s: %s", q, fatal);
		fail_connection(dc, line);
	} else if (!dc->closed && conn_queued_bytes(dc->conn) > MAX_UNREGISTERED_QUEUED_BYTES) {
		log_info(&dc->log, "closing connection: replies not read before registration");
		conn_close(dc->conn);
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
	downstream_maybe_register(dc);
}

void downstream_drain_input(struct downstream *dc) {
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

void downstream_on_message(struct conn *c, struct irc_msg *msg, void *data) {
	(void)c;
	struct downstream *dc = data;
	vec_push(&dc->pending_input, msg);
	downstream_drain_input(dc);
}

void downstream_input_done(struct downstream *dc) {
	if (dc->conn != nullptr) {
		conn_input_done(dc->conn);
	}
	if (dc->registered && !dc->closed) {
		dc->busy = false;
		downstream_drain_input(dc);
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
			// Late messages for a network the client just detached are
			// expected. The reply (an ACK for PING/PONG) carries the
			// message's label.
			downstream_begin_command(dc, msg);
			if (!cstr_eq(msg->command, "PING") && !cstr_eq(msg->command, "PONG")) {
				struct irc_msg *fail =
					IRC_MSG("FAIL", msg->command, "NETWORK_NOT_ATTACHED", tag,
						"Network not attached");
				server_set_prefix(dc->srv, fail);
				downstream_send(dc, fail);
			}
			downstream_end_command(dc);
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
		AUTOFREE char *fatal = finish_message(dc, downstream_handle_cap(dc, msg));
		if (fatal != nullptr) {
			fail_connection(dc, fatal);
		}
		if (!dc->closed && dc->events.updated != nullptr) {
			dc->events.updated(dc, dc->events.data);
		}
	} else if (cstr_eq(msg->command, "ISUPPORT")) {
		begin_message(dc, msg);
		downstream_send_isupport(dc, &dc->isupport);
		downstream_end_command(dc);
	} else if (cstr_eq(msg->command, "QUIT")) {
		downstream_close(dc);
	} else if (dc->events.message != nullptr) {
		struct downstream *source = dc->has_parent ? downstream_lookup(dc->parent_id) : dc;
		if (source != nullptr) {
			source->busy = true;
		}
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
