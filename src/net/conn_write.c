// Writing to connections: the write queue, rate limiting, line formatting
// and shutdown
#include "net/conn.h"

#include <errno.h>
#include <stdlib.h>
#include <string.h>

#include "base/alloc.h"
#include "base/utf8.h"
#include "net/conn_internal.h"

static void on_write_timeout(uv_timer_t *t) {
	struct conn *c = t->data;
	log_info(&c->log, "failed to write message: i/o timeout");
	conn_close(c);
}

static void on_shutdown(uv_shutdown_t *req, int status) {
	(void)status;
	struct conn *c = req->data;
	conn_close_handles(c);
	conn_unref(c);
}

// The queue is drained and a shutdown was asked: like soju-rs, the reader
// is cancelled, then the TLS session and the stream are shut down
static void finish_shutdown(struct conn *c) {
	c->shutting_down = true;
	bool was_closed = c->closed;
	if (!was_closed) {
		c->closed = true;
		if (c->reading) {
			uv_read_stop(c->stream);
			c->reading = false;
		}
	}
	if (c->has_tls && c->tls.handshake_done) {
		tls_session_shutdown(&c->tls);
		strbuf_reset(&c->write_buf);
		if (tls_session_take_output(&c->tls, &c->write_buf)) {
			conn_write_raw(c);
		}
	}
	if (!c->writing && !c->handles_closing) {
		conn_ref(c);
		c->shutdown_req.data = c;
		if (uv_shutdown(&c->shutdown_req, c->stream, on_shutdown) != 0) {
			conn_unref(c);
			conn_close_handles(c);
		}
	}
	if (!was_closed) {
		conn_notify_closed(c, nullptr);
	}
}

static void on_written(uv_write_t *req, int status) {
	struct conn *c = req->data;
	c->writing = false;
	if (!uv_is_closing((uv_handle_t *)&c->write_timer)) {
		uv_timer_stop(&c->write_timer);
	}
	strbuf_reset(&c->write_buf);
	if (status < 0) {
		if (status != UV_ECANCELED) {
			struct strbuf e = STRBUF_INIT;
			io_error_str(&e, status);
			log_info(&c->log, "failed to write message: %s", strbuf_cstr(&e));
			strbuf_free(&e);
		}
		conn_close(c);
	} else if (c->shutting_down) {
		if (!c->handles_closing) {
			conn_ref(c);
			c->shutdown_req.data = c;
			if (uv_shutdown(&c->shutdown_req, c->stream, on_shutdown) != 0) {
				conn_unref(c);
				conn_close_handles(c);
			}
		}
	} else {
		conn_flush_writes(c);
	}
	conn_unref(c);
}

// Writes write_buf (ciphertext or conn_plaintext) as is
void conn_write_raw(struct conn *c) {
	uv_buf_t buf = uv_buf_init(c->write_buf.buf, (unsigned)c->write_buf.len);
	c->write_req.data = c;
	conn_ref(c);
	int rc = uv_write(&c->write_req, c->stream, &buf, 1, on_written);
	if (rc != 0) {
		conn_unref(c);
		conn_close(c);
		return;
	}
	c->writing = true;
	uv_timer_start(&c->write_timer, on_write_timeout, CONN_WRITE_TIMEOUT_MS, 0);
}

// Appends conn_plaintext to write_buf, encrypting it with TLS
static bool add_bytes(struct conn *c, const char *p, size_t n) {
	if (!c->has_tls) {
		strbuf_append(&c->write_buf, p, n);
		return true;
	}
	if (tls_session_write(&c->tls, p, n) == TLS_ERROR) {
		log_info(&c->log, "failed to write message: %s", c->tls.error);
		return false;
	}
	(void)tls_session_take_output(&c->tls, &c->write_buf);
	return true;
}

// Appends a line (framed: with CRLF, or in a WebSocket message) to
// write_buf
static bool add_line(struct conn *c, const char *line) {
	log_debug(&c->log, "sent: %s", line);
	AUTO_STRBUF struct strbuf plain = STRBUF_INIT;
	conn_frame_line(c, line, &plain);
	return add_bytes(c, plain.buf, plain.len);
}

// Moves the raw bytes queued to write_buf
static bool add_raw(struct conn *c) {
	if (c->raw_out.len == 0) {
		return true;
	}
	c->queued_bytes -= c->raw_out.len;
	bool ok = add_bytes(c, c->raw_out.buf, c->raw_out.len);
	strbuf_reset(&c->raw_out);
	return ok;
}

static void on_rate_timer(uv_timer_t *t) {
	struct conn *c = t->data;
	c->rate_waiting = false;
	conn_flush_writes(c);
}

static bool rate_limited(const struct conn *c) {
	return c->rate_enabled && *c->rate_flag;
}

static char *dequeue(struct conn *c) {
	char *line = c->queue.data[c->queue_head];
	c->queue.data[c->queue_head++] = nullptr;
	c->queued_bytes -= strlen(line) + QUEUED_LINE_OVERHEAD;
	if (c->queue_head == c->queue.len) {
		c->queue.len = c->queue_head = 0;
	}
	return line;
}

void conn_flush_writes(struct conn *c) {
	if (c->writing || c->rate_waiting || c->handles_closing || c->shutting_down) {
		return;
	}
	strbuf_reset(&c->write_buf);
	if (c->has_tls) {
		// Handshake or alerts produced while a write was in flight
		(void)tls_session_take_output(&c->tls, &c->write_buf);
		if (!c->tls.handshake_done) {
			if (c->write_buf.len > 0) {
				conn_write_raw(c);
			}
			return;
		}
	}
	if (!add_raw(c)) {
		conn_close(c);
		return;
	}
	if (rate_limited(c)) {
		if (c->rate_line == nullptr && c->queue_head < c->queue.len) {
			c->rate_line = dequeue(c);
			uint64_t wait = rate_limiter_reserve(&c->limiter, uv_hrtime());
			if (wait > 0) {
				c->rate_waiting = true;
				// The loop's cached time may lag behind
				uv_update_time(c->stream->loop);
				uv_timer_start(&c->rate_timer, on_rate_timer,
					       (wait + 999999) / 1000000, 0);
				return;
			}
		}
		if (c->rate_line != nullptr) {
			char *line = c->rate_line;
			c->rate_line = nullptr;
			bool ok = add_line(c, line);
			xfree(line);
			if (!ok) {
				conn_close(c);
				return;
			}
		}
	} else {
		// Batch every line ready, e.g. backlog
		if (c->rate_line != nullptr) {
			char *line = c->rate_line;
			c->rate_line = nullptr;
			bool ok = add_line(c, line);
			xfree(line);
			if (!ok) {
				conn_close(c);
				return;
			}
		}
		while (c->queue_head < c->queue.len) {
			char *line = dequeue(c);
			bool ok = add_line(c, line);
			xfree(line);
			if (!ok) {
				conn_close(c);
				return;
			}
		}
	}
	if (c->write_buf.len == 0 && c->shutdown_requested && c->queue_head == c->queue.len &&
	    c->rate_line == nullptr) {
		// A WebSocket peer gets a close frame first
		AUTO_STRBUF struct strbuf frame = STRBUF_INIT;
		if (conn_frame_close(c, &frame) && !add_bytes(c, frame.buf, frame.len)) {
			conn_close(c);
			return;
		}
		if (c->write_buf.len == 0) {
			finish_shutdown(c);
			return;
		}
	}
	if (c->write_buf.len > 0) {
		conn_write_raw(c);
	}
}

static bool has_line_break(struct str s) {
	return memchr(s.p, '\r', s.len) != nullptr || memchr(s.p, '\n', s.len) != nullptr;
}

static void replace_breaks(char *s) {
	for (; *s != '\0'; s++) {
		if (*s == '\r' || *s == '\n') {
			*s = ' ';
		}
	}
}

void conn_format_line(const struct irc_msg *msg, struct strbuf *out) {
	size_t start = out->len;
	irc_msg_format(msg, out);
	if (!has_line_break(str_slice(strbuf_str(out), start, out->len))) {
		return;
	}
	// Multiline messages must be split before being sent: never let a line
	// break inject another message. Replace line breaks in the message
	// rather than in the line, so that a last parameter which gains spaces
	// is still formatted as a trailing one.
	struct irc_msg *m = irc_msg_clone(msg);
	AUTOFREE char *cmd = xstrdup(m->command);
	replace_breaks(cmd);
	irc_msg_set_command(m, cmd);
	for (size_t i = 0; i < m->params.len; i++) {
		AUTOFREE char *p = xstrdup(m->params.data[i]);
		replace_breaks(p);
		irc_msg_set_param(m, i, p);
	}
	if (m->prefix != nullptr) {
		AUTOFREE char *n = xstrdup(m->prefix->name);
		AUTOFREE char *u = xstrdup(m->prefix->user);
		AUTOFREE char *h = xstrdup(m->prefix->host);
		replace_breaks(n);
		replace_breaks(u);
		replace_breaks(h);
		irc_msg_set_prefix(m, n, u, h);
	}
	strbuf_truncate(out, start);
	irc_msg_format(m, out);
	irc_msg_free(m);
	// Tag values are escaped, but not tag keys
	for (size_t i = start; i < out->len; i++) {
		if (out->buf[i] == '\r' || out->buf[i] == '\n') {
			out->buf[i] = ' ';
		}
	}
}

static void enqueue_line(struct conn *c, char *line) {
	size_t size = strlen(line) + QUEUED_LINE_OVERHEAD;
	if (c->queued_bytes + size > MAX_QUEUED_BYTES) {
		xfree(line);
		log_info(&c->log, "closing connection: too many messages waiting to be sent");
		conn_close(c);
		return;
	}
	c->queued_bytes += size;
	strvec_push_take(&c->queue, line);
	conn_flush_writes(c);
}

void conn_send_raw(struct conn *c, const void *p, size_t n) {
	if (c->closed || c->shutdown_requested) {
		return;
	}
	if (c->queued_bytes + n > MAX_QUEUED_BYTES) {
		log_info(&c->log, "closing connection: too many messages waiting to be sent");
		conn_close(c);
		return;
	}
	c->queued_bytes += n;
	strbuf_append(&c->raw_out, n > 0 ? p : "", n);
	conn_flush_writes(c);
}

void conn_set_framing(struct conn *c, enum conn_framing f) {
	c->framing = f;
	ws_reader_finish(&c->ws);
	ws_reader_init(&c->ws, f == CONN_FRAMING_WEBSOCKET);
}

void conn_send(struct conn *c, const struct irc_msg *msg) {
	if (c->closed || c->shutdown_requested) {
		return;
	}
	struct strbuf line = STRBUF_INIT;
	conn_format_line(msg, &line);
	enqueue_line(c, strbuf_take(&line));
}

void conn_shutdown(struct conn *c) {
	if (c->closed || c->shutdown_requested) {
		return;
	}
	c->shutdown_requested = true;
	conn_flush_writes(c);
}
