// Reading from connections: line parsing, delivery under the in-flight
// limit, TLS input
#include "net/conn.h"

#include <errno.h>
#include <stdlib.h>
#include <string.h>

#include "base/alloc.h"
#include "base/utf8.h"
#include "net/conn_internal.h"

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

void conn_input_done(struct conn *c) {
	if (c->in_flight > 0) {
		c->in_flight--;
	}
	if (!c->closed) {
		conn_ref(c);
		conn_process(c);
		conn_update_reading(c);
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

void conn_become_open(struct conn *c) {
	c->stage = STAGE_OPEN;
	uv_timer_stop(&c->setup_timer);
	if (c->cb.ready != nullptr) {
		c->cb.ready(c, c->cb_data);
	}
	conn_flush_writes(c);
}

void conn_process(struct conn *c) {
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
		conn_become_open(c);
	}
	// The raw callback may switch to another framing: conn_process the rest of
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

void conn_plaintext(struct conn *c, const char *p, size_t n) {
	strbuf_append(c->stage == STAGE_PROXY ? &c->pre : &c->lines, p, n);
}

void conn_flush_tls_output(struct conn *c) {
	if (c->writing || c->handles_closing) {
		// Sent with the next write
		return;
	}
	strbuf_reset(&c->write_buf);
	if (tls_session_take_output(&c->tls, &c->write_buf)) {
		conn_write_raw(c);
	}
}

static void after_handshake(struct conn *c) {
	c->has_peer_cert = tls_session_peer_cert(&c->tls, &c->peer_cert);
	if (c->has_peer_cert) {
		strbuf_append(&c->peer_cert, "", 0);
	}
	c->stage = c->proxy ? STAGE_PROXY : STAGE_OPEN;
	if (c->stage == STAGE_OPEN) {
		conn_become_open(c);
	}
}

static void tls_input(struct conn *c, const char *p, size_t n) {
	tls_session_feed(&c->tls, p, n);
	if (!c->tls.handshake_done) {
		enum tls_status st = tls_session_handshake(&c->tls);
		conn_flush_tls_output(c);
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
	conn_plaintext(c, plain.buf != nullptr ? plain.buf : "", plain.len);
	strbuf_free(&plain);
	conn_flush_tls_output(c);
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
			conn_plaintext(c, buf->base, (size_t)nread);
		}
		conn_process(c);
	} else if (nread == UV_EOF) {
		c->eof = true;
		if (c->stage == STAGE_TLS) {
			conn_close(c);
		} else {
			conn_process(c);
		}
	} else if (nread < 0) {
		struct strbuf e = STRBUF_INIT;
		io_error_str(&e, (int)nread);
		conn_fail_msg(c, "%s", strbuf_cstr(&e));
		strbuf_free(&e);
	}
	xfree(buf->base);
	conn_update_reading(c);
	conn_unref(c);
}

void conn_update_reading(struct conn *c) {
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
