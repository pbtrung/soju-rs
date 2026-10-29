#include "http/websocket.h"

#include <openssl/evp.h>
#include <string.h>

#include "base/alloc.h"
#include "base/base64.h"
#include "base/utf8.h"
#include "net/conn.h"

// RFC 6455 section 1.3
static constexpr char WS_GUID[] = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
// Control frames carry at most 125 bytes (RFC 6455 section 5.5)
static constexpr size_t WS_MAX_CONTROL = 125;

char *ws_accept_key(struct str key) {
	struct strbuf in = STRBUF_INIT;
	strbuf_append_str(&in, key);
	strbuf_append(&in, WS_GUID, sizeof(WS_GUID) - 1);
	unsigned char digest[EVP_MAX_MD_SIZE];
	unsigned int n = 0;
	if (EVP_Digest(in.buf, in.len, digest, &n, EVP_sha1(), nullptr) != 1) {
		die_oom();
	}
	strbuf_free(&in);
	return base64_encode_cstr(BASE64_STANDARD, digest, n);
}

void ws_write_frame(struct strbuf *out, enum ws_opcode op, const void *payload, size_t len,
		    const uint8_t *mask) {
	uint8_t hdr[14];
	size_t n = 0;
	hdr[n++] = (uint8_t)(0x80U | (unsigned)op);
	unsigned mask_bit = mask != nullptr ? 0x80U : 0U;
	if (len < 126) {
		hdr[n++] = (uint8_t)(mask_bit | (unsigned)len);
	} else if (len <= 0xffff) {
		hdr[n++] = (uint8_t)(mask_bit | 126U);
		hdr[n++] = (uint8_t)(len >> 8U);
		hdr[n++] = (uint8_t)len;
	} else {
		hdr[n++] = (uint8_t)(mask_bit | 127U);
		for (int shift = 56; shift >= 0; shift -= 8) {
			hdr[n++] = (uint8_t)((uint64_t)len >> (unsigned)shift);
		}
	}
	if (mask != nullptr) {
		memcpy(hdr + n, mask, 4);
		n += 4;
	}
	strbuf_reserve(out, size_add(n, len));
	strbuf_append(out, (const char *)hdr, n);
	size_t start = out->len;
	strbuf_append(out, len > 0 ? payload : "", len);
	if (mask != nullptr) {
		for (size_t i = 0; i < len; i++) {
			out->buf[start + i] = (char)((uint8_t)out->buf[start + i] ^ mask[i % 4]);
		}
	}
}

void ws_write_close(struct strbuf *out, uint16_t code, const uint8_t *mask) {
	uint8_t p[2] = {(uint8_t)(code >> 8U), (uint8_t)code};
	ws_write_frame(out, WS_OP_CLOSE, p, code != 0 ? 2 : 0, mask);
}

void ws_reader_init(struct ws_reader *r, bool server) {
	*r = (struct ws_reader){.server = server};
}

void ws_reader_finish(struct ws_reader *r) {
	strbuf_free(&r->msg);
}

void ws_event_free(struct ws_event *ev) {
	strbuf_free(&ev->payload);
}

static enum ws_event_kind protocol_error(struct ws_event *ev, uint16_t code, const char *msg) {
	ev->kind = WS_ERROR;
	ev->error_code = code;
	ev->error = msg;
	return WS_ERROR;
}

// A parsed frame header
struct frame {
	bool fin;
	enum ws_opcode op;
	size_t header_len;
	size_t len;
	bool masked;
	uint8_t mask[4];
};

// WS_NEED_MORE until the header is complete, WS_ERROR on a violation,
// else WS_MESSAGE (meaning: a header was parsed)
static enum ws_event_kind parse_header(const struct ws_reader *r, struct str in, struct frame *f,
				       struct ws_event *ev) {
	if (in.len < 2) {
		return WS_NEED_MORE;
	}
	const uint8_t *p = (const uint8_t *)in.p;
	f->fin = (p[0] & 0x80U) != 0;
	if ((p[0] & 0x70U) != 0) {
		return protocol_error(ev, WS_CLOSE_PROTOCOL, "reserved bits are non-zero");
	}
	f->op = (enum ws_opcode)(p[0] & 0x0fU);
	f->masked = (p[1] & 0x80U) != 0;
	uint64_t len = p[1] & 0x7fU;
	size_t n = 2;
	if (len == 126) {
		if (in.len < 4) {
			return WS_NEED_MORE;
		}
		len = ((uint64_t)p[2] << 8U) | p[3];
		n = 4;
	} else if (len == 127) {
		if (in.len < 10) {
			return WS_NEED_MORE;
		}
		len = 0;
		for (size_t i = 2; i < 10; i++) {
			len = (len << 8U) | p[i];
		}
		n = 10;
	}
	switch (f->op) {
	case WS_OP_CONTINUATION:
	case WS_OP_TEXT:
	case WS_OP_BINARY:
		if (len > MAX_LINE_LENGTH) {
			return protocol_error(ev, WS_CLOSE_TOO_BIG, "message too long");
		}
		break;
	case WS_OP_CLOSE:
	case WS_OP_PING:
	case WS_OP_PONG:
		if (!f->fin) {
			return protocol_error(ev, WS_CLOSE_PROTOCOL, "fragmented control frame");
		}
		if (len > WS_MAX_CONTROL) {
			return protocol_error(ev, WS_CLOSE_PROTOCOL, "control frame too big");
		}
		break;
	default:
		return protocol_error(ev, WS_CLOSE_PROTOCOL, "unknown opcode");
	}
	if (f->masked != r->server) {
		return protocol_error(ev, WS_CLOSE_PROTOCOL,
				      r->server ? "received an unmasked frame from client"
						: "received a masked frame from server");
	}
	if (f->masked) {
		if (in.len < n + 4) {
			return WS_NEED_MORE;
		}
		memcpy(f->mask, p + n, 4);
		n += 4;
	}
	f->header_len = n;
	f->len = (size_t)len;
	return WS_MESSAGE;
}

// Appends a frame's payload, unmasked
static void append_payload(struct strbuf *out, const struct frame *f, const char *p) {
	size_t start = out->len;
	strbuf_append(out, f->len > 0 ? p : "", f->len);
	if (f->masked) {
		for (size_t i = 0; i < f->len; i++) {
			out->buf[start + i] = (char)((uint8_t)out->buf[start + i] ^ f->mask[i % 4]);
		}
	}
}

static enum ws_event_kind control_frame(const struct frame *f, const char *p, struct ws_event *ev) {
	struct strbuf payload = STRBUF_INIT;
	append_payload(&payload, f, p);
	if (f->op == WS_OP_CLOSE) {
		ev->close_code = -1;
		if (payload.len == 1) {
			strbuf_free(&payload);
			return protocol_error(ev, WS_CLOSE_PROTOCOL, "invalid close frame");
		}
		if (payload.len >= 2) {
			ev->close_code = (int)(((unsigned)(uint8_t)payload.buf[0] << 8U) |
					       (unsigned)(uint8_t)payload.buf[1]);
		}
		strbuf_free(&payload);
		ev->kind = WS_CLOSE;
		return WS_CLOSE;
	}
	ev->payload = payload;
	ev->kind = f->op == WS_OP_PING ? WS_PING : WS_PONG;
	return ev->kind;
}

// A data frame: appends it to the message being assembled. Returns
// WS_MESSAGE once complete, WS_NEED_MORE otherwise.
static enum ws_event_kind data_frame(struct ws_reader *r, const struct frame *f, const char *p,
				     struct ws_event *ev) {
	if (f->op == WS_OP_CONTINUATION) {
		if (!r->fragmented) {
			return protocol_error(ev, WS_CLOSE_PROTOCOL,
					      "continuation frame without a message");
		}
	} else {
		if (r->fragmented) {
			return protocol_error(ev, WS_CLOSE_PROTOCOL,
					      "new message while a fragmented one is incomplete");
		}
		r->fragmented = true;
		r->binary = f->op == WS_OP_BINARY;
		strbuf_reset(&r->msg);
	}
	if (r->msg.len + f->len > MAX_LINE_LENGTH) {
		return protocol_error(ev, WS_CLOSE_TOO_BIG, "message too long");
	}
	append_payload(&r->msg, f, p);
	if (!f->fin) {
		return WS_NEED_MORE;
	}
	r->fragmented = false;
	if (!r->binary && !utf8_valid(strbuf_str(&r->msg))) {
		return protocol_error(ev, WS_CLOSE_PROTOCOL, "invalid UTF-8 in a text message");
	}
	ev->kind = WS_MESSAGE;
	ev->binary = r->binary;
	ev->payload = r->msg;
	r->msg = STRBUF_INIT;
	return WS_MESSAGE;
}

enum ws_event_kind ws_read(struct ws_reader *r, struct str in, size_t *consumed,
			   struct ws_event *ev) {
	*ev = (struct ws_event){.kind = WS_NEED_MORE, .close_code = -1};
	*consumed = 0;
	for (;;) {
		struct str rest = str_slice(in, *consumed, in.len);
		struct frame f;
		enum ws_event_kind k = parse_header(r, rest, &f, ev);
		if (k != WS_MESSAGE) {
			return k;
		}
		if (rest.len - f.header_len < f.len) {
			return WS_NEED_MORE;
		}
		const char *p = rest.p + f.header_len;
		*consumed += f.header_len + f.len;
		if (f.op == WS_OP_CLOSE || f.op == WS_OP_PING || f.op == WS_OP_PONG) {
			return control_frame(&f, p, ev);
		}
		k = data_frame(r, &f, p, ev);
		if (k != WS_NEED_MORE) {
			return k;
		}
	}
}
