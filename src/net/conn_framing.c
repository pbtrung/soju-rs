// The raw and WebSocket framings of connections (soju-rs conn.rs Reader::Ws
// and Writer::Ws; tokio-tungstenite answered pings and close frames)
#include <openssl/rand.h>
#include <string.h>

#include "base/alloc.h"
#include "net/conn_internal.h"

static bool is_websocket(const struct conn *c) {
	return c->framing == CONN_FRAMING_WEBSOCKET || c->framing == CONN_FRAMING_WEBSOCKET_CLIENT;
}

// A client masks its frames with a random key; nullptr for a server
static const uint8_t *frame_mask(const struct conn *c, uint8_t buf[4]) {
	if (c->framing != CONN_FRAMING_WEBSOCKET_CLIENT) {
		return nullptr;
	}
	if (RAND_bytes(buf, 4) != 1) {
		memset(buf, 0x5a, 4);
	}
	return buf;
}

void conn_frame_line(struct conn *c, const char *line, struct strbuf *out) {
	if (!is_websocket(c)) {
		strbuf_append_cstr(out, line);
		strbuf_append(out, "\r\n", 2);
		return;
	}
	uint8_t mask[4];
	ws_write_frame(out, WS_OP_TEXT, line, strlen(line), frame_mask(c, mask));
}

bool conn_frame_close(struct conn *c, struct strbuf *out) {
	if (!is_websocket(c) || c->ws_close_sent) {
		return false;
	}
	c->ws_close_sent = true;
	uint8_t mask[4];
	ws_write_close(out, WS_CLOSE_NORMAL, frame_mask(c, mask));
	return true;
}

void conn_process_raw(struct conn *c) {
	if (c->cb.raw == nullptr) {
		if (c->eof) {
			conn_close(c);
		}
		return;
	}
	if (c->lines.len == 0 && !c->eof) {
		return;
	}
	c->cb.raw(c, &c->lines, c->eof, c->cb_data);
}

static void pong(struct conn *c, struct str payload) {
	uint8_t mask[4];
	AUTO_STRBUF struct strbuf frame = STRBUF_INIT;
	ws_write_frame(&frame, WS_OP_PONG, payload.p, payload.len, frame_mask(c, mask));
	conn_send_raw(c, frame.buf, frame.len);
}

// A text or binary message: one IRC line. Returns false if the connection
// failed.
static bool message(struct conn *c, struct ws_event *ev) {
	struct irc_msg *msg;
	struct err *e = nullptr;
	// Binary messages are decoded lossily, like text ones
	if (!conn_parse_line(strbuf_str(&ev->payload), &msg, &e)) {
		conn_fail(c, e);
		err_free(e);
		return false;
	}
	if (msg == nullptr) {
		conn_fail_msg(c, "irc: cannot parse zero-length message");
		return false;
	}
	conn_deliver(c, msg);
	return true;
}

// The peer closed the WebSocket: like soju-rs, a normal closure is an EOF,
// other status codes an error. The close frame is answered once the
// messages read are processed.
static bool peer_close(struct conn *c, int code) {
	if (code != -1 && code != WS_CLOSE_NORMAL && code != WS_CLOSE_AWAY) {
		conn_fail_msg(c, "websocket closed with status %d", code);
		return false;
	}
	c->ws_peer_closed = true;
	c->eof = true;
	return true;
}

void conn_process_websocket(struct conn *c) {
	enum conn_framing f = c->framing;
	while (!c->closed && c->framing == f && c->in_flight < MAX_INPUT_IN_FLIGHT &&
	       !c->ws_peer_closed) {
		size_t used = 0;
		struct ws_event ev;
		enum ws_event_kind k = ws_read(&c->ws, strbuf_str(&c->lines), &used, &ev);
		strbuf_drop_front(&c->lines, used);
		bool ok = true;
		switch (k) {
		case WS_NEED_MORE:
			ws_event_free(&ev);
			if (c->eof && c->in_flight == 0) {
				// The stream ended without a close frame
				conn_close(c);
			}
			return;
		case WS_ERROR:
			conn_fail_msg(c, "WebSocket protocol error: %s", ev.error);
			ok = false;
			break;
		case WS_PING:
			pong(c, strbuf_str(&ev.payload));
			break;
		case WS_PONG:
			break;
		case WS_CLOSE:
			ok = peer_close(c, ev.close_code);
			break;
		case WS_MESSAGE:
			ok = message(c, &ev);
			break;
		}
		ws_event_free(&ev);
		if (!ok) {
			return;
		}
	}
	if (!c->closed && c->ws_peer_closed && c->in_flight == 0) {
		conn_shutdown(c);
	}
}
