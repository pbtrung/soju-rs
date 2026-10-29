// WebSocket framing (RFC 6455) for IRC over WebSocket (soju-rs used
// tokio-tungstenite): the handshake's accept key, frame encoding and a
// frame reader assembling messages. Limits match soju-rs's configuration:
// frames and messages of at most MAX_LINE_LENGTH bytes.
#pragma once

#include <stddef.h>
#include <stdint.h>

#include "base/str.h"

enum ws_opcode {
	WS_OP_CONTINUATION = 0x0,
	WS_OP_TEXT = 0x1,
	WS_OP_BINARY = 0x2,
	WS_OP_CLOSE = 0x8,
	WS_OP_PING = 0x9,
	WS_OP_PONG = 0xa,
};

// Close status codes
static constexpr uint16_t WS_CLOSE_NORMAL = 1000;
static constexpr uint16_t WS_CLOSE_AWAY = 1001;
static constexpr uint16_t WS_CLOSE_PROTOCOL = 1002;
static constexpr uint16_t WS_CLOSE_TOO_BIG = 1009;

// The Sec-WebSocket-Accept value for a Sec-WebSocket-Key (28 characters)
[[nodiscard]] char *ws_accept_key(struct str key);

// Appends a frame with FIN set. A client masks its frames: mask is the
// 4-byte masking key, or nullptr for a server.
void ws_write_frame(struct strbuf *out, enum ws_opcode op, const void *payload, size_t len,
		    const uint8_t *mask);
// Appends a close frame with a status code (0: no payload)
void ws_write_close(struct strbuf *out, uint16_t code, const uint8_t *mask);

enum ws_event_kind {
	WS_NEED_MORE, // no complete frame yet
	WS_MESSAGE, // a complete text or binary message
	WS_PING,
	WS_PONG,
	WS_CLOSE,
	WS_ERROR, // protocol violation: the connection must be closed
};

struct ws_event {
	enum ws_event_kind kind;
	bool binary; // WS_MESSAGE: a binary message
	struct strbuf payload; // WS_MESSAGE, WS_PING, WS_PONG (owned)
	int close_code; // WS_CLOSE: the status code, or -1 without one
	uint16_t error_code; // WS_ERROR: the close code to send
	const char *error; // WS_ERROR: a static message
};

// Reads frames from a peer: a server reads masked frames, a client
// unmasked ones. Fragmented messages are assembled; control frames may
// come between fragments.
struct ws_reader {
	bool server;
	bool fragmented; // a message's first fragment was read
	bool binary; // of the message being assembled
	struct strbuf msg;
};

void ws_reader_init(struct ws_reader *r, bool server);
void ws_reader_finish(struct ws_reader *r);
// Reads at most one event from in. *consumed is set to the number of bytes
// used (possibly some even with WS_NEED_MORE, for fragments). The caller
// frees ev->payload.
[[nodiscard]] enum ws_event_kind ws_read(struct ws_reader *r, struct str in, size_t *consumed,
					 struct ws_event *ev);
void ws_event_free(struct ws_event *ev);
