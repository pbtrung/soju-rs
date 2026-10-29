// Ported from soju-rs src/conn.rs (websocket_wire_format) and src/listen.rs
// (websocket_large_lines), plus C-only tests of the WebSocket and raw
// framings of connections: pings, close frames, framing switches
#include "testsupport.h"

#include <sys/socket.h>
#include <unistd.h>

#include "base/alloc.h"
#include "http/websocket.h"
#include "net/conn.h"

static struct log_root root;
static struct logger logger;

struct peer {
	struct conn *c;
	irc_msg_vec got;
	bool closed;
	char *close_err;
	struct strbuf raw; // raw framing: bytes received
	bool switch_on_newline; // raw framing: switch to WebSocket after "\n"
};

static void on_message(struct conn *c, struct irc_msg *msg, void *data) {
	struct peer *p = data;
	vec_push(&p->got, msg);
	conn_input_done(c);
}

static void on_closed(struct conn *c, const struct err *err, void *data) {
	(void)c;
	struct peer *p = data;
	p->closed = true;
	if (err != nullptr) {
		p->close_err = xstrdup(err_msg(err));
	}
}

static void on_raw(struct conn *c, struct strbuf *in, bool eof, void *data) {
	struct peer *p = data;
	ptrdiff_t nl = str_index(strbuf_str(in), '\n');
	if (p->switch_on_newline && nl >= 0) {
		strbuf_append(&p->raw, in->buf, (size_t)nl + 1);
		strbuf_drop_front(in, (size_t)nl + 1);
		conn_set_framing(c, CONN_FRAMING_WEBSOCKET);
		return;
	}
	if (eof) {
		conn_close(c);
	}
}

static const struct conn_callbacks callbacks = {
	.message = on_message, .closed = on_closed, .raw = on_raw};

static void peer_init(struct peer *p, uv_loop_t *loop, int fd, enum conn_framing f) {
	*p = (struct peer){0};
	uv_pipe_t *pipe = xcalloc(1, sizeof(*pipe));
	assert_int_equal(uv_pipe_init(loop, pipe, 0), 0);
	assert_int_equal(uv_pipe_open(pipe, fd), 0);
	p->c = conn_new((uv_stream_t *)pipe, &(struct conn_options){.framing = f}, &logger,
			&callbacks, p);
}

static void peer_free(struct peer *p) {
	conn_unref(p->c);
	irc_msg_vec_free(&p->got);
	xfree(p->close_err);
	strbuf_free(&p->raw);
}

#define RUN_UNTIL(loop, cond, ms)                                            \
	do {                                                                 \
		uint64_t deadline_ = uv_hrtime() + (uint64_t)(ms) * 1000000; \
		while (!(cond) && uv_hrtime() < deadline_) {                 \
			uv_run((loop), UV_RUN_NOWAIT);                       \
		}                                                            \
	} while (0)

static void finish_loop(uv_loop_t *loop) {
	uv_run(loop, UV_RUN_DEFAULT);
	if (uv_loop_close(loop) != 0) {
		uv_print_all_handles(loop, stderr);
		fail_msg("handles left open");
	}
}

// Reads from a raw socket until a WebSocket event other than NEED_MORE
static enum ws_event_kind read_event(uv_loop_t *loop, int fd, struct ws_reader *r,
				     struct strbuf *buf, struct ws_event *ev) {
	uint64_t deadline = uv_hrtime() + 2000000000ULL;
	for (;;) {
		size_t used;
		enum ws_event_kind k = ws_read(r, strbuf_str(buf), &used, ev);
		strbuf_drop_front(buf, used);
		if (k != WS_NEED_MORE || uv_hrtime() > deadline) {
			return k;
		}
		uv_run(loop, UV_RUN_NOWAIT);
		char tmp[4096];
		ssize_t n = recv(fd, tmp, sizeof(tmp), MSG_DONTWAIT);
		if (n > 0) {
			strbuf_append(buf, tmp, (size_t)n);
		} else if (n == 0) {
			return WS_NEED_MORE;
		}
	}
}

static void send_frame(int fd, enum ws_opcode op, const char *p, size_t n) {
	static const uint8_t mask[4] = {1, 2, 3, 4};
	AUTO_STRBUF struct strbuf f = STRBUF_INIT;
	ws_write_frame(&f, op, p, n, mask);
	assert_true(write(fd, f.buf, f.len) == (ssize_t)f.len);
}

static void setup(uv_loop_t *loop, int fds[2]) {
	log_init(&logger, &root);
	uv_loop_init(loop);
	assert_int_equal(socketpair(AF_UNIX, SOCK_STREAM, 0, fds), 0);
}

static void test_websocket_wire_format(void **state) {
	(void)state;
	uv_loop_t loop;
	int fds[2];
	setup(&loop, fds);
	struct peer s;
	peer_init(&s, &loop, fds[0], CONN_FRAMING_WEBSOCKET);
	struct irc_msg *msg = IRC_MSG("PRIVMSG", "#a", "hello world");
	irc_msg_set_prefix(msg, "nick", "user", "host");
	irc_tags_set(&msg->tags, "time", "2020-01-01T00:00:00.000Z");
	irc_tags_set(&msg->tags, "msgid", "a b;c");
	conn_send(s.c, msg);
	// One text frame per message, without CRLF
	struct ws_reader r;
	ws_reader_init(&r, false);
	AUTO_STRBUF struct strbuf buf = STRBUF_INIT;
	struct ws_event ev;
	assert_int_equal(read_event(&loop, fds[1], &r, &buf, &ev), WS_MESSAGE);
	assert_false(ev.binary);
	AUTOFREE char *want = irc_msg_to_string(msg);
	assert_sb_eq(&ev.payload, want);
	ws_event_free(&ev);
	ws_reader_finish(&r);
	irc_msg_free(msg);
	close(fds[1]);
	RUN_UNTIL(&loop, s.closed, 2000);
	peer_free(&s);
	finish_loop(&loop);
	log_free(&logger);
}

// Lines bigger than the WebSocket buffers still go through, both ways
static void test_websocket_large_lines(void **state) {
	(void)state;
	uv_loop_t loop;
	int fds[2];
	setup(&loop, fds);
	struct peer s, c;
	peer_init(&s, &loop, fds[0], CONN_FRAMING_WEBSOCKET);
	peer_init(&c, &loop, fds[1], CONN_FRAMING_WEBSOCKET_CLIENT);
	char text[2 * 8192 + 1];
	memset(text, 'x', sizeof(text) - 1);
	text[sizeof(text) - 1] = '\0';
	struct irc_msg *m = IRC_MSG("PRIVMSG", "#a", text);
	conn_send(c.c, m);
	conn_send(s.c, m);
	irc_msg_free(m);
	RUN_UNTIL(&loop, s.got.len == 1 && c.got.len == 1, 2000);
	assert_int_equal(s.got.len, 1);
	assert_int_equal(c.got.len, 1);
	assert_string_equal(irc_msg_param(s.got.data[0], 1), text);
	assert_string_equal(irc_msg_param(c.got.data[0], 1), text);
	// A graceful shutdown sends a close frame: the peer sees an EOF
	conn_shutdown(c.c);
	RUN_UNTIL(&loop, s.closed && c.closed, 2000);
	assert_true(s.closed);
	assert_null(s.close_err);
	peer_free(&s);
	peer_free(&c);
	finish_loop(&loop);
	log_free(&logger);
}

static void test_websocket_ping_close(void **state) {
	(void)state;
	uv_loop_t loop;
	int fds[2];
	setup(&loop, fds);
	struct peer s;
	peer_init(&s, &loop, fds[0], CONN_FRAMING_WEBSOCKET);
	send_frame(fds[1], WS_OP_PING, "abc", 3);
	struct ws_reader r;
	ws_reader_init(&r, false);
	AUTO_STRBUF struct strbuf buf = STRBUF_INIT;
	struct ws_event ev;
	assert_int_equal(read_event(&loop, fds[1], &r, &buf, &ev), WS_PONG);
	assert_sb_eq(&ev.payload, "abc");
	ws_event_free(&ev);
	// Messages before the close frame are delivered; the close frame is
	// answered, then the stream is shut down
	send_frame(fds[1], WS_OP_TEXT, "PING x", 6);
	AUTO_STRBUF struct strbuf f = STRBUF_INIT;
	ws_write_close(&f, WS_CLOSE_NORMAL, (const uint8_t[]){9, 9, 9, 9});
	assert_true(write(fds[1], f.buf, f.len) == (ssize_t)f.len);
	assert_int_equal(read_event(&loop, fds[1], &r, &buf, &ev), WS_CLOSE);
	assert_int_equal(ev.close_code, WS_CLOSE_NORMAL);
	RUN_UNTIL(&loop, s.closed, 2000);
	assert_int_equal(s.got.len, 1);
	assert_null(s.close_err);
	ws_reader_finish(&r);
	close(fds[1]);
	peer_free(&s);
	finish_loop(&loop);
	log_free(&logger);
}

static void test_websocket_errors(void **state) {
	(void)state;
	static const struct {
		uint16_t code;
		const char *frame; // a text frame, or nullptr for a close frame
		const char *err;
	} cases[] = {
		{WS_CLOSE_PROTOCOL, nullptr, "websocket closed with status 1002"},
		{0, "", "irc: cannot parse zero-length message"},
		{0, "\r\nPING x", "irc: cannot parse zero-length message"},
	};
	for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
		uv_loop_t loop;
		int fds[2];
		setup(&loop, fds);
		struct peer s;
		peer_init(&s, &loop, fds[0], CONN_FRAMING_WEBSOCKET);
		if (cases[i].frame == nullptr) {
			AUTO_STRBUF struct strbuf f = STRBUF_INIT;
			ws_write_close(&f, cases[i].code, (const uint8_t[]){1, 2, 3, 4});
			assert_true(write(fds[1], f.buf, f.len) == (ssize_t)f.len);
		} else {
			send_frame(fds[1], WS_OP_TEXT, cases[i].frame, strlen(cases[i].frame));
		}
		RUN_UNTIL(&loop, s.closed, 2000);
		assert_cstr_eq(s.close_err, cases[i].err);
		close(fds[1]);
		peer_free(&s);
		finish_loop(&loop);
		log_free(&logger);
	}
	// An unmasked frame from a client
	uv_loop_t loop;
	int fds[2];
	setup(&loop, fds);
	struct peer s;
	peer_init(&s, &loop, fds[0], CONN_FRAMING_WEBSOCKET);
	AUTO_STRBUF struct strbuf f = STRBUF_INIT;
	ws_write_frame(&f, WS_OP_TEXT, "PING x", 6, nullptr);
	assert_true(write(fds[1], f.buf, f.len) == (ssize_t)f.len);
	RUN_UNTIL(&loop, s.closed, 2000);
	assert_cstr_eq(s.close_err,
		       "WebSocket protocol error: received an unmasked frame from client");
	close(fds[1]);
	peer_free(&s);
	finish_loop(&loop);
	log_free(&logger);
}

// Input after an upgrade request, read with it, is parsed as frames
static void test_raw_then_websocket(void **state) {
	(void)state;
	uv_loop_t loop;
	int fds[2];
	setup(&loop, fds);
	struct peer s;
	peer_init(&s, &loop, fds[0], CONN_FRAMING_RAW);
	s.switch_on_newline = true;
	AUTO_STRBUF struct strbuf in = STRBUF_INIT;
	strbuf_append_cstr(&in, "UPGRADE\n");
	ws_write_frame(&in, WS_OP_TEXT, "PING a", 6, (const uint8_t[]){5, 6, 7, 8});
	assert_true(write(fds[1], in.buf, in.len) == (ssize_t)in.len);
	RUN_UNTIL(&loop, s.got.len == 1, 2000);
	assert_sb_eq(&s.raw, "UPGRADE\n");
	assert_int_equal(s.got.len, 1);
	assert_msg_str(s.got.data[0], "PING a");
	// Raw bytes queued before messages are written first
	conn_send_raw(s.c, "HTTP\0x", 6);
	struct irc_msg *m = IRC_MSG("PONG", "a");
	conn_send(s.c, m);
	irc_msg_free(m);
	char buf[64];
	size_t got = 0;
	uint64_t deadline = uv_hrtime() + 2000000000ULL;
	while (got < 6 + 8 && uv_hrtime() < deadline) {
		uv_run(&loop, UV_RUN_NOWAIT);
		ssize_t n = recv(fds[1], buf + got, sizeof(buf) - got, MSG_DONTWAIT);
		if (n > 0) {
			got += (size_t)n;
		}
	}
	assert_int_equal(got, 6 + 8);
	assert_memory_equal(buf, "HTTP\0x\x81\x06PONG a", 14);
	close(fds[1]);
	RUN_UNTIL(&loop, s.closed, 2000);
	peer_free(&s);
	finish_loop(&loop);
	log_free(&logger);
}

T_MAIN(cmocka_unit_test(test_websocket_wire_format), cmocka_unit_test(test_websocket_large_lines),
       cmocka_unit_test(test_websocket_ping_close), cmocka_unit_test(test_websocket_errors),
       cmocka_unit_test(test_raw_then_websocket))
