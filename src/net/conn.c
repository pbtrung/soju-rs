// Connections: reference counting, closing, the setup timeout and views
#include "net/conn.h"

#include <errno.h>
#include <stdlib.h>
#include <string.h>

#include "base/alloc.h"
#include "base/utf8.h"
#include "net/conn_internal.h"

static _Thread_local struct conn *connections;

void io_error_str(struct strbuf *out, int uv_err) {
	if (uv_err == UV_EOF) {
		strbuf_append_cstr(out, "unexpected end of file");
		return;
	}
	if (uv_err == UV_ETIMEDOUT) {
		strbuf_append_cstr(out, "i/o timeout");
		return;
	}
	strbuf_printf(out, "%s (os error %d)", strerror(-uv_err), -uv_err);
}

void conn_ref(struct conn *c) {
	c->refs++;
}

static void conn_free(struct conn *c) {
	if (c->prev != nullptr) {
		c->prev->next = c->next;
	} else {
		connections = c->next;
	}
	if (c->next != nullptr) {
		c->next->prev = c->prev;
	}
	tls_session_finish(&c->tls);
	strbuf_free(&c->peer_cert);
	proxy_header_free(&c->proxy_header);
	strbuf_free(&c->pre);
	strbuf_free(&c->lines);
	ws_reader_finish(&c->ws);
	strbuf_free(&c->raw_out);
	strvec_free(&c->queue);
	strbuf_free(&c->write_buf);
	xfree(c->rate_line);
	log_free(&c->log);
	ref_untrack(&c->ref);
	xfree(c);
}

void conn_unref(struct conn *c) {
	if (c == nullptr) {
		return;
	}
	if (c->refs == 1 && !c->handles_closing) {
		// The last reference: close first; the handles hold references
		// until their close callbacks
		conn_close(c);
	}
	if (--c->refs == 0) {
		conn_free(c);
	}
}

void conn_notify_closed(struct conn *c, const struct err *err) {
	if (c->closed_notified) {
		return;
	}
	c->closed_notified = true;
	if (c->cb.closed != nullptr) {
		c->cb.closed(c, err, c->cb_data);
	}
}

static void on_handle_closed(uv_handle_t *h) {
	struct conn *c = h->data;
	if (h == (uv_handle_t *)c->stream) {
		xfree(c->stream);
		c->stream = nullptr;
	}
	c->handles_open--;
	if (c->handles_open == 0) {
		log_debug(&c->log, "connection closed");
	}
	conn_unref(c);
}

static void close_handle(struct conn *c, uv_handle_t *h) {
	if (!uv_is_closing(h)) {
		c->refs++;
		uv_close(h, on_handle_closed);
	}
}

void conn_close_handles(struct conn *c) {
	if (c->handles_closing) {
		return;
	}
	c->handles_closing = true;
	close_handle(c, (uv_handle_t *)&c->write_timer);
	close_handle(c, (uv_handle_t *)&c->rate_timer);
	close_handle(c, (uv_handle_t *)&c->setup_timer);
	close_handle(c, (uv_handle_t *)c->stream);
}

// Stops everything: reading, timers, queued writes. err is reported to
// the closed callback.
void conn_fail(struct conn *c, const struct err *err) {
	if (c->closed) {
		return;
	}
	c->closed = true;
	conn_ref(c);
	if (c->reading) {
		uv_read_stop(c->stream);
		c->reading = false;
	}
	conn_close_handles(c);
	conn_notify_closed(c, err);
	conn_unref(c);
}

void conn_fail_msg(struct conn *c, const char *fmt, ...) {
	struct err e = {0};
	va_list ap;
	va_start(ap, fmt);
	struct strbuf sb = STRBUF_INIT;
	strbuf_vprintf(&sb, fmt, ap);
	va_end(ap);
	e.msg = strbuf_take(&sb);
	conn_fail(c, &e);
	xfree(e.msg);
}

void conn_close(struct conn *c) {
	conn_fail(c, nullptr);
}

bool conn_is_closed(const struct conn *c) {
	return c->closed;
}

size_t conn_queued_bytes(const struct conn *c) {
	return c->queued_bytes;
}

size_t conn_queued(const struct conn *c) {
	return c->queue.len - c->queue_head + (c->rate_line != nullptr ? 1 : 0) +
	       (c->raw_out.len > 0 ? 1 : 0);
}

const struct proxy_header *conn_proxy_header(const struct conn *c) {
	return &c->proxy_header;
}

const struct strbuf *conn_peer_cert(const struct conn *c) {
	return c->has_peer_cert ? &c->peer_cert : nullptr;
}

struct str conn_alpn(const struct conn *c) {
	return c->has_tls ? tls_session_alpn(&c->tls) : STR_EMPTY;
}

uv_stream_t *conn_stream(const struct conn *c) {
	return c->stream;
}

void conn_set_callbacks(struct conn *c, const struct conn_callbacks *cb, void *data) {
	c->cb = *cb;
	c->cb_data = data;
}

static void on_setup_timeout(uv_timer_t *t) {
	struct conn *c = t->data;
	if (c->stage != STAGE_START) {
		conn_close(c);
		return;
	}
	conn_ref(c);
	conn_become_open(c);
	conn_process(c);
	conn_update_reading(c);
	conn_unref(c);
}

struct conn *conn_new(uv_stream_t *stream, const struct conn_options *opts,
		      const struct logger *log, const struct conn_callbacks *cb, void *data) {
	struct conn *c = xcalloc(1, sizeof(*c));
	REF_TRACK(&c->ref, "conn");
	c->next = connections;
	if (connections != nullptr) {
		connections->prev = c;
	}
	connections = c;
	c->refs = 1;
	c->stream = stream;
	stream->data = c;
	log_with_prefix(&c->log, log, "");
	if (cb != nullptr) {
		c->cb = *cb;
	}
	c->cb_data = data;
	uv_loop_t *loop = stream->loop;
	uv_timer_init(loop, &c->write_timer);
	uv_timer_init(loop, &c->rate_timer);
	uv_timer_init(loop, &c->setup_timer);
	c->write_timer.data = c->rate_timer.data = c->setup_timer.data = c;
	c->handles_open = 4;
	c->proxy = opts->proxy;
	c->framing = opts->framing;
	ws_reader_init(&c->ws, c->framing == CONN_FRAMING_WEBSOCKET);
	if (opts->rate != nullptr) {
		c->rate_enabled = true;
		c->rate_flag = opts->rate->enabled;
		rate_limiter_init(&c->limiter, opts->rate->delay_ns, opts->rate->burst,
				  uv_hrtime());
	}
	log_debug(&c->log, "new connection");
	bool ok = true;
	if (opts->tls_server != nullptr) {
		ok = tls_session_init_server(&c->tls, opts->tls_server);
		c->has_tls = ok;
	} else if (opts->tls_client != nullptr) {
		ok = tls_session_init_client(&c->tls, opts->tls_client, opts->tls_host);
		c->has_tls = ok;
	}
	c->stage = c->has_tls ? STAGE_TLS : (c->proxy ? STAGE_PROXY : STAGE_OPEN);
	if (opts->setup_timeout_ms > 0 && c->stage != STAGE_OPEN) {
		uv_timer_start(&c->setup_timer, on_setup_timeout, opts->setup_timeout_ms, 0);
	}
	if (!ok) {
		conn_fail_msg(c, "failed to create a TLS session");
		return c;
	}
	if (c->has_tls && opts->tls_client != nullptr) {
		// The client speaks first
		conn_ref(c);
		enum tls_status st = tls_session_handshake(&c->tls);
		conn_flush_tls_output(c);
		if (st == TLS_ERROR) {
			conn_fail_msg(c, "tls: %s", c->tls.error);
		}
		conn_unref(c);
	}
	if (c->stage == STAGE_OPEN && !c->closed) {
		// No setup step: ready from the next loop iteration, once the
		// caller has the pointer
		c->stage = STAGE_START;
		uv_timer_start(&c->setup_timer, on_setup_timeout, 0, 0);
	}
	conn_update_reading(c);
	return c;
}

// Views

struct conn_view {
	struct ref_node ref;
	struct conn *c;
	char *tag_key, *tag_value;
	bool closed;
	void (*on_close)(void *data);
	void *data;
};

struct conn_view *conn_view_new(struct conn *c, const char *tag_key, const char *tag_value,
				void (*on_close)(void *data), void *data) {
	struct conn_view *v = xcalloc(1, sizeof(*v));
	REF_TRACK(&v->ref, "conn_view");
	conn_ref(c);
	v->c = c;
	v->tag_key = xstrdup(tag_key);
	v->tag_value = xstrdup(tag_value);
	v->on_close = on_close;
	v->data = data;
	return v;
}

void conn_view_free(struct conn_view *v) {
	if (v == nullptr) {
		return;
	}
	xfree(v->tag_key);
	xfree(v->tag_value);
	struct conn *c = v->c;
	ref_untrack(&v->ref);
	xfree(v);
	conn_unref(c);
}

bool conn_view_is_closed(const struct conn_view *v) {
	return conn_is_closed(v->c) || v->closed;
}

void conn_view_send(struct conn_view *v, const struct irc_msg *msg) {
	if (conn_view_is_closed(v) || v->c->shutdown_requested) {
		return;
	}
	struct irc_msg *m = irc_msg_clone(msg);
	irc_tags_set(&m->tags, v->tag_key, v->tag_value);
	conn_send(v->c, m);
	irc_msg_free(m);
}

static void view_close(struct conn_view *v, bool notify) {
	if (conn_view_is_closed(v)) {
		v->closed = true;
		return;
	}
	if (notify) {
		struct irc_msg *m = IRC_MSG("ERROR", "Closing link");
		conn_view_send(v, m);
		irc_msg_free(m);
	}
	if (!v->closed) {
		v->closed = true;
		if (v->on_close != nullptr) {
			v->on_close(v->data);
		}
	}
}

void conn_view_close(struct conn_view *v) {
	view_close(v, true);
}

void conn_view_close_quietly(struct conn_view *v) {
	view_close(v, false);
}

void conn_close_all(void) {
	VEC(struct conn *) snapshot = {0};
	for (struct conn *c = connections; c != nullptr; c = c->next) {
		conn_ref(c);
		vec_push(&snapshot, c);
	}
	for (size_t i = 0; i < snapshot.len; i++) {
		conn_close(snapshot.data[i]);
		conn_unref(snapshot.data[i]);
	}
	vec_free(&snapshot);
}
