#include "net/conn.h"

#include <errno.h>
#include <stdlib.h>
#include <string.h>

#include "base/alloc.h"
#include "base/utf8.h"
#include "net/conn_internal.h"

static _Thread_local struct conn *connections;

static void process(struct conn *c);
static void close_handles(struct conn *c);

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

static void notify_closed(struct conn *c, const struct err *err) {
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

static void close_handles(struct conn *c) {
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
	close_handles(c);
	notify_closed(c, err);
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

// Writing

static void on_write_timeout(uv_timer_t *t) {
	struct conn *c = t->data;
	log_info(&c->log, "failed to write message: i/o timeout");
	conn_close(c);
}

static void write_raw(struct conn *c);

static void on_shutdown(uv_shutdown_t *req, int status) {
	(void)status;
	struct conn *c = req->data;
	close_handles(c);
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
			write_raw(c);
		}
	}
	if (!c->writing && !c->handles_closing) {
		conn_ref(c);
		c->shutdown_req.data = c;
		if (uv_shutdown(&c->shutdown_req, c->stream, on_shutdown) != 0) {
			conn_unref(c);
			close_handles(c);
		}
	}
	if (!was_closed) {
		notify_closed(c, nullptr);
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
				close_handles(c);
			}
		}
	} else {
		conn_flush_writes(c);
	}
	conn_unref(c);
}

// Writes write_buf (ciphertext or plaintext) as is
static void write_raw(struct conn *c) {
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

// Appends plaintext to write_buf, encrypting it with TLS
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
				write_raw(c);
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
		write_raw(c);
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

// Reading

bool conn_parse_line(struct str line, struct irc_msg **msg, struct err **err) {
	*msg = nullptr;
	size_t end = 0;
	while (end < line.len && line.p[end] != '\r' && line.p[end] != '\n') {
		end++;
	}
	line = str_slice(line, 0, end);
	if (line.len == 0) {
		return true;
	}
	AUTO_STRBUF struct strbuf text = STRBUF_INIT;
	utf8_lossy(&text, line);
	// C strings can't hold NUL bytes: they become U+FFFD (soju-rs keeps
	// them)
	if (memchr(text.buf, '\0', text.len) != nullptr) {
		struct strbuf clean = STRBUF_INIT;
		for (size_t i = 0; i < text.len; i++) {
			if (text.buf[i] == '\0') {
				utf8_push(&clean, UTF8_REPLACEMENT);
			} else {
				strbuf_append_char(&clean, text.buf[i]);
			}
		}
		strbuf_free(&text);
		text = clean;
	}
	enum irc_parse_error pe;
	*msg = irc_msg_parse(strbuf_str(&text), &pe);
	if (*msg == nullptr) {
		return err_set(err, 0, "%s", irc_parse_error_str(pe));
	}
	return true;
}

static void update_reading(struct conn *c);

void conn_input_done(struct conn *c) {
	if (c->in_flight > 0) {
		c->in_flight--;
	}
	if (!c->closed) {
		conn_ref(c);
		process(c);
		update_reading(c);
		conn_unref(c);
	}
}

// Delivers the complete lines buffered, while under the in-flight limit
static void process_lines(struct conn *c) {
	while (!c->closed && c->in_flight < MAX_INPUT_IN_FLIGHT) {
		const char *nl =
			c->lines.len > 0 ? memchr(c->lines.buf, '\n', c->lines.len) : nullptr;
		if (nl == nullptr) {
			if (c->lines.len > MAX_LINE_LENGTH) {
				conn_fail_msg(c, "line too long");
			}
			return;
		}
		size_t len = (size_t)(nl - c->lines.buf) + 1;
		if (len > MAX_LINE_LENGTH) {
			conn_fail_msg(c, "line too long");
			return;
		}
		struct irc_msg *msg;
		struct err *e = nullptr;
		bool ok = conn_parse_line(str_new(c->lines.buf, len), &msg, &e);
		memmove(c->lines.buf, c->lines.buf + len, c->lines.len - len);
		strbuf_truncate(&c->lines, c->lines.len - len);
		if (!ok) {
			conn_fail(c, e);
			err_free(e);
			return;
		}
		if (msg == nullptr) {
			// It's valid for a message to be empty, ignore these
			continue;
		}
		conn_deliver(c, msg);
	}
}

void conn_deliver(struct conn *c, struct irc_msg *msg) {
	if (log_debug_enabled(&c->log)) {
		AUTOFREE char *s = irc_msg_to_string(msg);
		log_printf(&c->log, "received: %s", s);
	}
	c->in_flight++;
	if (c->cb.message != nullptr) {
		c->cb.message(c, msg, c->cb_data);
	} else {
		irc_msg_free(msg);
		c->in_flight--;
	}
}

static void become_open(struct conn *c) {
	c->stage = STAGE_OPEN;
	uv_timer_stop(&c->setup_timer);
	if (c->cb.ready != nullptr) {
		c->cb.ready(c, c->cb_data);
	}
	conn_flush_writes(c);
}

static void process(struct conn *c) {
	if (c->closed) {
		return;
	}
	if (c->stage == STAGE_PROXY) {
		size_t consumed = 0;
		struct err *e = nullptr;
		switch (proxy_parse(strbuf_str(&c->pre), c->eof, &c->proxy_header, &consumed, &e)) {
		case PROXY_NEED_MORE:
			return;
		case PROXY_ERROR:
			conn_fail(c, e);
			err_free(e);
			return;
		case PROXY_NONE:
			consumed = 0;
			break;
		case PROXY_HEADER:
			break;
		}
		strbuf_append_str(&c->lines, str_slice(strbuf_str(&c->pre), consumed, c->pre.len));
		strbuf_free(&c->pre);
		become_open(c);
	}
	// The raw callback may switch to another framing: process the rest of
	// the input with it
	enum conn_framing f;
	do {
		if (c->stage != STAGE_OPEN || c->closed) {
			return;
		}
		f = c->framing;
		switch (f) {
		case CONN_FRAMING_LINES:
			process_lines(c);
			if (!c->closed && c->eof && c->in_flight == 0) {
				// EOF; a line without its end is dropped
				conn_close(c);
			}
			break;
		case CONN_FRAMING_RAW:
			conn_process_raw(c);
			break;
		case CONN_FRAMING_WEBSOCKET:
		case CONN_FRAMING_WEBSOCKET_CLIENT:
			conn_process_websocket(c);
			break;
		}
	} while (c->framing != f);
}

static void plaintext(struct conn *c, const char *p, size_t n) {
	strbuf_append(c->stage == STAGE_PROXY ? &c->pre : &c->lines, p, n);
}

static void flush_tls_output(struct conn *c) {
	if (c->writing || c->handles_closing) {
		// Sent with the next write
		return;
	}
	strbuf_reset(&c->write_buf);
	if (tls_session_take_output(&c->tls, &c->write_buf)) {
		write_raw(c);
	}
}

static void after_handshake(struct conn *c) {
	c->has_peer_cert = tls_session_peer_cert(&c->tls, &c->peer_cert);
	if (c->has_peer_cert) {
		strbuf_append(&c->peer_cert, "", 0);
	}
	c->stage = c->proxy ? STAGE_PROXY : STAGE_OPEN;
	if (c->stage == STAGE_OPEN) {
		become_open(c);
	}
}

static void tls_input(struct conn *c, const char *p, size_t n) {
	tls_session_feed(&c->tls, p, n);
	if (!c->tls.handshake_done) {
		enum tls_status st = tls_session_handshake(&c->tls);
		flush_tls_output(c);
		if (st == TLS_ERROR) {
			conn_fail_msg(c, "tls: %s", c->tls.error);
			return;
		}
		if (!c->tls.handshake_done) {
			return;
		}
		after_handshake(c);
		if (c->closed) {
			return;
		}
	}
	struct strbuf plain = STRBUF_INIT;
	enum tls_status st = tls_session_read(&c->tls, &plain);
	plaintext(c, plain.buf != nullptr ? plain.buf : "", plain.len);
	strbuf_free(&plain);
	flush_tls_output(c);
	if (st == TLS_ERROR) {
		conn_fail_msg(c, "tls: %s", c->tls.error);
	} else if (st == TLS_CLOSED) {
		c->eof = true;
	}
}

static void on_alloc(uv_handle_t *h, size_t suggested, uv_buf_t *buf) {
	(void)h;
	(void)suggested;
	buf->base = xmalloc(16384);
	buf->len = 16384;
}

static void on_read(uv_stream_t *s, ssize_t nread, const uv_buf_t *buf) {
	struct conn *c = s->data;
	conn_ref(c);
	if (nread > 0) {
		if (c->has_tls) {
			tls_input(c, buf->base, (size_t)nread);
		} else {
			plaintext(c, buf->base, (size_t)nread);
		}
		process(c);
	} else if (nread == UV_EOF) {
		c->eof = true;
		if (c->stage == STAGE_TLS) {
			conn_close(c);
		} else {
			process(c);
		}
	} else if (nread < 0) {
		struct strbuf e = STRBUF_INIT;
		io_error_str(&e, (int)nread);
		conn_fail_msg(c, "%s", strbuf_cstr(&e));
		strbuf_free(&e);
	}
	xfree(buf->base);
	update_reading(c);
	conn_unref(c);
}

static void update_reading(struct conn *c) {
	bool want = !c->closed && !c->eof && c->in_flight < MAX_INPUT_IN_FLIGHT;
	if (want && !c->reading) {
		if (uv_read_start(c->stream, on_alloc, on_read) == 0) {
			c->reading = true;
		}
	} else if (!want && c->reading) {
		uv_read_stop(c->stream);
		c->reading = false;
	}
}

static void on_setup_timeout(uv_timer_t *t) {
	struct conn *c = t->data;
	if (c->stage != STAGE_START) {
		conn_close(c);
		return;
	}
	conn_ref(c);
	become_open(c);
	process(c);
	update_reading(c);
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
		flush_tls_output(c);
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
	update_reading(c);
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
