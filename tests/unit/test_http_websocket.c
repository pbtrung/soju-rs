// C-only tests of the WebSocket codec (soju-rs relied on tungstenite):
// RFC 6455 handshake key, framing, fragmentation and protocol errors
#include "testsupport.h"

#include "base/alloc.h"
#include "http/websocket.h"
#include "net/conn.h"

static const uint8_t mask[4] = {0x37, 0xfa, 0x21, 0x3d};

static void test_accept_key(void **state) {
	(void)state;
	// RFC 6455 section 1.3
	AUTOFREE char *k = ws_accept_key(STR_LIT("dGhlIHNhbXBsZSBub25jZQ=="));
	assert_string_equal(k, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
}

static void test_frame_lengths(void **state) {
	(void)state;
	// RFC 6455 section 5.7: a masked "Hello"
	AUTO_STRBUF struct strbuf out = STRBUF_INIT;
	ws_write_frame(&out, WS_OP_TEXT, "Hello", 5, mask);
	static const uint8_t want[] = {0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d,
				       0x7f, 0x9f, 0x4d, 0x51, 0x58};
	assert_int_equal(out.len, sizeof(want));
	assert_memory_equal(out.buf, want, sizeof(want));
	size_t lens[] = {0, 125, 126, 65535, 65536};
	size_t hdr[] = {2, 2, 4, 4, 10};
	for (size_t i = 0; i < 5; i++) {
		strbuf_reset(&out);
		char *p = xcalloc(1, lens[i] + 1);
		ws_write_frame(&out, WS_OP_BINARY, p, lens[i], nullptr);
		assert_int_equal(out.len, hdr[i] + lens[i]);
		xfree(p);
	}
}

// Reads one event from a buffer holding exactly the frames given
static enum ws_event_kind read_one(struct ws_reader *r, struct strbuf *in, struct ws_event *ev) {
	size_t used = 0;
	enum ws_event_kind k = ws_read(r, strbuf_str(in), &used, ev);
	strbuf_drop_front(in, used);
	return k;
}

static void test_masked_roundtrip(void **state) {
	(void)state;
	struct ws_reader r;
	ws_reader_init(&r, true);
	AUTO_STRBUF struct strbuf in = STRBUF_INIT;
	ws_write_frame(&in, WS_OP_TEXT, "PRIVMSG #a :hi", 14, mask);
	// Byte by byte: nothing until the frame is complete
	AUTO_STRBUF struct strbuf partial = STRBUF_INIT;
	struct ws_event ev;
	for (size_t i = 0; i + 1 < in.len; i++) {
		strbuf_append(&partial, in.buf + i, 1);
		size_t used;
		assert_int_equal(ws_read(&r, strbuf_str(&partial), &used, &ev), WS_NEED_MORE);
		assert_int_equal(used, 0);
	}
	assert_int_equal(read_one(&r, &in, &ev), WS_MESSAGE);
	assert_false(ev.binary);
	assert_sb_eq(&ev.payload, "PRIVMSG #a :hi");
	assert_int_equal(in.len, 0);
	ws_event_free(&ev);
	ws_reader_finish(&r);
}

static void test_fragments_and_control(void **state) {
	(void)state;
	struct ws_reader r;
	ws_reader_init(&r, true);
	AUTO_STRBUF struct strbuf in = STRBUF_INIT;
	// "ab" + ping + "cd": the ping comes between the fragments
	strbuf_append_char(&in, 0x01); // TEXT without FIN
	strbuf_append_char(&in, (char)0x82);
	strbuf_append(&in, (const char *)mask, 4);
	strbuf_append_char(&in, (char)((unsigned)'a' ^ mask[0]));
	strbuf_append_char(&in, (char)((unsigned)'b' ^ mask[1]));
	ws_write_frame(&in, WS_OP_PING, "p", 1, mask);
	strbuf_append_char(&in, (char)0x80); // CONTINUATION with FIN
	strbuf_append_char(&in, (char)0x82);
	strbuf_append(&in, (const char *)mask, 4);
	strbuf_append_char(&in, (char)((unsigned)'c' ^ mask[0]));
	strbuf_append_char(&in, (char)((unsigned)'d' ^ mask[1]));
	struct ws_event ev;
	assert_int_equal(read_one(&r, &in, &ev), WS_PING);
	assert_sb_eq(&ev.payload, "p");
	ws_event_free(&ev);
	assert_int_equal(read_one(&r, &in, &ev), WS_MESSAGE);
	assert_sb_eq(&ev.payload, "abcd");
	ws_event_free(&ev);
	ws_write_close(&in, WS_CLOSE_AWAY, mask);
	assert_int_equal(read_one(&r, &in, &ev), WS_CLOSE);
	assert_int_equal(ev.close_code, WS_CLOSE_AWAY);
	ws_write_frame(&in, WS_OP_CLOSE, "", 0, mask);
	assert_int_equal(read_one(&r, &in, &ev), WS_CLOSE);
	assert_int_equal(ev.close_code, -1);
	ws_reader_finish(&r);
}

static void expect_error(bool server, const struct strbuf *in, uint16_t code) {
	struct ws_reader r;
	ws_reader_init(&r, server);
	struct ws_event ev;
	size_t used;
	assert_int_equal(ws_read(&r, strbuf_str(in), &used, &ev), WS_ERROR);
	assert_int_equal(ev.error_code, code);
	assert_non_null(ev.error);
	ws_event_free(&ev);
	ws_reader_finish(&r);
}

static void test_protocol_errors(void **state) {
	(void)state;
	AUTO_STRBUF struct strbuf in = STRBUF_INIT;
	// A server needs masked frames, a client unmasked ones
	ws_write_frame(&in, WS_OP_TEXT, "x", 1, nullptr);
	expect_error(true, &in, WS_CLOSE_PROTOCOL);
	strbuf_reset(&in);
	ws_write_frame(&in, WS_OP_TEXT, "x", 1, mask);
	expect_error(false, &in, WS_CLOSE_PROTOCOL);
	// Too big: the header is enough to know
	strbuf_reset(&in);
	char *big = xcalloc(1, MAX_LINE_LENGTH + 1);
	ws_write_frame(&in, WS_OP_TEXT, big, MAX_LINE_LENGTH + 1, mask);
	xfree(big);
	strbuf_truncate(&in, 14);
	expect_error(true, &in, WS_CLOSE_TOO_BIG);
	// Reserved bits, unknown opcode, fragmented ping, big ping
	static const struct {
		const char *p;
		size_t n;
	} bad[] = {{"\xc1\x80", 2}, {"\x83\x80", 2}, {"\x09\x80", 2}, {"\x89\xfe\x00\x7e", 4}};
	for (size_t i = 0; i < 4; i++) {
		strbuf_reset(&in);
		strbuf_append(&in, bad[i].p, bad[i].n);
		strbuf_append(&in, (const char *)mask, 4);
		expect_error(true, &in, WS_CLOSE_PROTOCOL);
	}
	// A continuation without a message, a one-byte close payload
	strbuf_reset(&in);
	ws_write_frame(&in, WS_OP_CONTINUATION, "x", 1, mask);
	expect_error(true, &in, WS_CLOSE_PROTOCOL);
	strbuf_reset(&in);
	ws_write_frame(&in, WS_OP_CLOSE, "x", 1, mask);
	expect_error(true, &in, WS_CLOSE_PROTOCOL);
	// Invalid UTF-8 in a text message, but not in a binary one
	strbuf_reset(&in);
	ws_write_frame(&in, WS_OP_TEXT, "\xff", 1, mask);
	expect_error(true, &in, WS_CLOSE_PROTOCOL);
	struct ws_reader r;
	ws_reader_init(&r, true);
	strbuf_reset(&in);
	ws_write_frame(&in, WS_OP_BINARY, "\xff", 1, mask);
	struct ws_event ev;
	assert_int_equal(read_one(&r, &in, &ev), WS_MESSAGE);
	assert_true(ev.binary);
	ws_event_free(&ev);
	ws_reader_finish(&r);
}

static void test_message_limit(void **state) {
	(void)state;
	// Fragments adding up to more than the limit
	struct ws_reader r;
	ws_reader_init(&r, true);
	AUTO_STRBUF struct strbuf in = STRBUF_INIT;
	char *half = xcalloc(1, MAX_LINE_LENGTH / 2 + 1);
	memset(half, 'x', MAX_LINE_LENGTH / 2 + 1);
	ws_write_frame(&in, WS_OP_TEXT, half, MAX_LINE_LENGTH / 2 + 1, mask);
	in.buf[0] = 0x01; // no FIN
	ws_write_frame(&in, WS_OP_CONTINUATION, half, MAX_LINE_LENGTH / 2 + 1, mask);
	xfree(half);
	struct ws_event ev;
	assert_int_equal(read_one(&r, &in, &ev), WS_ERROR);
	assert_int_equal(ev.error_code, WS_CLOSE_TOO_BIG);
	ws_reader_finish(&r);
}

T_MAIN(cmocka_unit_test(test_accept_key), cmocka_unit_test(test_frame_lengths),
       cmocka_unit_test(test_masked_roundtrip), cmocka_unit_test(test_fragments_and_control),
       cmocka_unit_test(test_protocol_errors), cmocka_unit_test(test_message_limit))
