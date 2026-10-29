// Ported from soju-rs src/conn.rs tests (websocket_wire_format waits for
// phase 8), plus C-only tests of the line reader limits
#include "testsupport.h"

#include <sys/socket.h>
#include <unistd.h>

#include "base/alloc.h"
#include "net/conn.h"

static struct log_root root;
static struct logger logger;

struct peer {
	struct conn *c;
	irc_msg_vec got;
	bool closed;
	char *close_err;
	bool auto_done; // call conn_input_done right away
};

static void on_message(struct conn *c, struct irc_msg *msg, void *data) {
	struct peer *p = data;
	vec_push(&p->got, msg);
	if (p->auto_done) {
		conn_input_done(c);
	}
}

static void on_closed(struct conn *c, const struct err *err, void *data) {
	(void)c;
	struct peer *p = data;
	p->closed = true;
	if (err != nullptr) {
		p->close_err = xstrdup(err_msg(err));
	}
}

static const struct conn_callbacks callbacks = {.message = on_message, .closed = on_closed};

static uv_stream_t *pipe_on(uv_loop_t *loop, int fd) {
	uv_pipe_t *p = xcalloc(1, sizeof(*p));
	assert_int_equal(uv_pipe_init(loop, p, 0), 0);
	assert_int_equal(uv_pipe_open(p, fd), 0);
	return (uv_stream_t *)p;
}

static void socket_pair(int fds[2]) {
	assert_int_equal(socketpair(AF_UNIX, SOCK_STREAM, 0, fds), 0);
}

static void peer_init(struct peer *p, uv_loop_t *loop, int fd, const struct conn_options *opts) {
	*p = (struct peer){.auto_done = true};
	static const struct conn_options none = {0};
	p->c = conn_new(pipe_on(loop, fd), opts != nullptr ? opts : &none, &logger, &callbacks, p);
}

static void peer_free(struct peer *p) {
	conn_unref(p->c);
	irc_msg_vec_free(&p->got);
	xfree(p->close_err);
}

// Runs the loop until cond or a timeout (ms)
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

static void setup_log(void) {
	log_init(&logger, &root);
}

static void test_stream_roundtrip(void **state) {
	(void)state;
	setup_log();
	uv_loop_t loop;
	uv_loop_init(&loop);
	int fds[2];
	socket_pair(fds);
	struct peer a, b;
	peer_init(&a, &loop, fds[0], nullptr);
	peer_init(&b, &loop, fds[1], nullptr);
	struct irc_msg *m = IRC_MSG("PRIVMSG", "#a", "hello world");
	conn_send(a.c, m);
	irc_msg_free(m);
	m = IRC_MSG("PING", "x");
	conn_send(a.c, m);
	irc_msg_free(m);
	RUN_UNTIL(&loop, b.got.len == 2, 2000);
	assert_int_equal(b.got.len, 2);
	assert_msg_str(b.got.data[0], "PRIVMSG #a :hello world");
	assert_string_equal(b.got.data[1]->command, "PING");
	conn_shutdown(a.c);
	RUN_UNTIL(&loop, b.closed, 2000);
	assert_true(b.closed);
	assert_null(b.close_err);
	conn_close(b.c);
	peer_free(&a);
	peer_free(&b);
	finish_loop(&loop);
	log_free(&logger);
}

static void test_rate_limited(void **state) {
	(void)state;
	setup_log();
	uv_loop_t loop;
	uv_loop_init(&loop);
	int fds[2];
	socket_pair(fds);
	// The Rust test pauses Tokio's clock with a 2 s delay; this one waits
	// for real, with 200 ms
	bool enabled = true;
	struct conn_rate rate = {.delay_ns = 200000000, .burst = 2, .enabled = &enabled};
	struct conn_options opts = {.rate = &rate};
	struct peer a, b;
	peer_init(&a, &loop, fds[0], &opts);
	peer_init(&b, &loop, fds[1], nullptr);
	uint64_t start = uv_hrtime();
	for (int i = 0; i < 3; i++) {
		char p[12];
		snprintf(p, sizeof(p), "%d", i);
		struct irc_msg *m = IRC_MSG("PING", p);
		conn_send(a.c, m);
		irc_msg_free(m);
	}
	RUN_UNTIL(&loop, b.got.len == 3, 3000);
	assert_int_equal(b.got.len, 3);
	assert_true(uv_hrtime() - start >= 199000000);
	conn_close(a.c);
	conn_close(b.c);
	peer_free(&a);
	peer_free(&b);
	finish_loop(&loop);
	log_free(&logger);
}

static struct irc_msg *tagged_message(void) {
	struct irc_msg *m = IRC_MSG("PRIVMSG", "#a", "hello world");
	irc_msg_set_prefix(m, "nick", "user", "host");
	irc_tags_set(&m->tags, "time", "2020-01-01T00:00:00.000Z");
	irc_tags_set(&m->tags, "msgid", "a b;c");
	return m;
}

static void line_eq(const struct irc_msg *m, const char *want) {
	struct strbuf sb = STRBUF_INIT;
	conn_format_line(m, &sb);
	assert_sb_eq(&sb, want);
	strbuf_free(&sb);
}

static void test_queued_line(void **state) {
	(void)state;
	struct irc_msg *msg = tagged_message();
	char *s = irc_msg_to_string(msg);
	line_eq(msg, s);
	xfree(s);
	irc_msg_free(msg);
	// Line breaks can't inject another message
	msg = IRC_MSG("PRIVMSG", "#a", "x\r\nQUIT");
	line_eq(msg, "PRIVMSG #a :x  QUIT");
	irc_msg_free(msg);
	msg = irc_msg_parse(STR_LIT("PRIVMSG #a :x  QUIT"), nullptr);
	assert_string_equal(msg->params.data[1], "x  QUIT");
	irc_msg_free(msg);
	msg = IRC_MSG("PRIVMSG", "#a\n", "x");
	irc_tags_set(&msg->tags, "a\nb", "c\n");
	line_eq(msg, "@a b=c\\n PRIVMSG #a  x");
	irc_msg_free(msg);
}

static void test_stream_wire_format(void **state) {
	(void)state;
	setup_log();
	uv_loop_t loop;
	uv_loop_init(&loop);
	int fds[2];
	socket_pair(fds);
	struct peer a;
	peer_init(&a, &loop, fds[0], nullptr);
	struct irc_msg *msg = tagged_message();
	conn_send(a.c, msg);
	struct irc_msg *ping = IRC_MSG("PING", "x");
	conn_send(a.c, ping);
	irc_msg_free(ping);
	conn_shutdown(a.c);
	RUN_UNTIL(&loop, a.closed && conn_queued(a.c) == 0, 2000);
	uv_run(&loop, UV_RUN_DEFAULT);
	char buf[512];
	ssize_t n = read(fds[1], buf, sizeof(buf));
	assert_true(n > 0);
	char *s = irc_msg_to_string(msg);
	AUTOFREE char *want = xasprintf("%s\r\nPING x\r\n", s);
	assert_int_equal((size_t)n, strlen(want));
	assert_memory_equal(buf, want, (size_t)n);
	xfree(s);
	irc_msg_free(msg);
	close(fds[1]);
	peer_free(&a);
	finish_loop(&loop);
	log_free(&logger);
}

static void test_queue_budget(void **state) {
	(void)state;
	setup_log();
	uv_loop_t loop;
	uv_loop_init(&loop);
	int fds[2];
	socket_pair(fds);
	// The peer never reads
	struct peer a;
	peer_init(&a, &loop, fds[0], nullptr);
	char text[401];
	memset(text, 'x', 400);
	text[400] = '\0';
	size_t n = MAX_QUEUED_BYTES / (400 + QUEUED_LINE_OVERHEAD) + 1;
	struct irc_msg *m = IRC_MSG("PRIVMSG", "#a", text);
	for (size_t i = 0; i < n; i++) {
		conn_send(a.c, m);
	}
	irc_msg_free(m);
	assert_true(conn_is_closed(a.c));
	close(fds[1]);
	peer_free(&a);
	finish_loop(&loop);
	log_free(&logger);
}

static void test_input_in_flight(void **state) {
	(void)state;
	setup_log();
	uv_loop_t loop;
	uv_loop_init(&loop);
	int fds[2];
	socket_pair(fds);
	struct peer a, b;
	peer_init(&a, &loop, fds[0], nullptr);
	peer_init(&b, &loop, fds[1], nullptr);
	b.auto_done = false;
	for (size_t i = 0; i < MAX_INPUT_IN_FLIGHT + 1; i++) {
		char p[8];
		snprintf(p, sizeof(p), "%zu", i);
		struct irc_msg *m = IRC_MSG("PING", p);
		conn_send(a.c, m);
		irc_msg_free(m);
	}
	RUN_UNTIL(&loop, b.got.len == MAX_INPUT_IN_FLIGHT, 2000);
	// The next one isn't delivered until a permit is released
	RUN_UNTIL(&loop, false, 50);
	assert_int_equal(b.got.len, MAX_INPUT_IN_FLIGHT);
	conn_input_done(b.c);
	RUN_UNTIL(&loop, b.got.len == MAX_INPUT_IN_FLIGHT + 1, 2000);
	assert_int_equal(b.got.len, MAX_INPUT_IN_FLIGHT + 1);
	char want[8];
	snprintf(want, sizeof(want), "%zu", MAX_INPUT_IN_FLIGHT);
	assert_string_equal(irc_msg_param(vec_last(&b.got), 0), want);
	conn_close(a.c);
	conn_close(b.c);
	peer_free(&a);
	peer_free(&b);
	finish_loop(&loop);
	log_free(&logger);
}

static int view_closed_count;

static void on_view_close(void *data) {
	(void)data;
	view_closed_count++;
}

static void test_view(void **state) {
	(void)state;
	setup_log();
	uv_loop_t loop;
	uv_loop_init(&loop);
	int fds[2];
	socket_pair(fds);
	struct peer c, b;
	peer_init(&c, &loop, fds[0], nullptr);
	peer_init(&b, &loop, fds[1], nullptr);
	view_closed_count = 0;
	struct conn_view *v = conn_view_new(c.c, "soju.im/network", "1", on_view_close, nullptr);
	struct irc_msg *m = IRC_MSG("PING", "x");
	irc_tags_set(&m->tags, "label", "a");
	conn_view_send(v, m);
	irc_msg_free(m);
	RUN_UNTIL(&loop, b.got.len == 1, 2000);
	assert_string_equal(irc_tags_get(&b.got.data[0]->tags, "soju.im/network"), "1");
	assert_string_equal(irc_tags_get(&b.got.data[0]->tags, "label"), "a");

	// Closing the view tells the peer, and leaves the connection open
	conn_view_close(v);
	conn_view_close(v);
	assert_true(conn_view_is_closed(v) && !conn_is_closed(c.c));
	assert_int_equal(view_closed_count, 1);
	RUN_UNTIL(&loop, b.got.len == 2, 2000);
	assert_string_equal(b.got.data[1]->command, "ERROR");
	assert_string_equal(irc_tags_get(&b.got.data[1]->tags, "soju.im/network"), "1");
	m = IRC_MSG("PING", "dropped");
	conn_view_send(v, m);
	irc_msg_free(m);
	m = IRC_MSG("PING", "y");
	conn_send(c.c, m);
	irc_msg_free(m);
	RUN_UNTIL(&loop, b.got.len == 3, 2000);
	assert_string_equal(irc_msg_param(b.got.data[2], 0), "y");
	assert_null(irc_tags_get(&b.got.data[2]->tags, "soju.im/network"));

	// Closed quietly: no ERROR
	struct conn_view *q = conn_view_new(c.c, "soju.im/network", "2", nullptr, nullptr);
	conn_view_close_quietly(q);
	m = IRC_MSG("PING", "z");
	conn_send(c.c, m);
	irc_msg_free(m);
	RUN_UNTIL(&loop, b.got.len == 4, 2000);
	assert_string_equal(irc_msg_param(b.got.data[3], 0), "z");

	// Closing the connection closes its views
	struct conn_view *w = conn_view_new(c.c, "soju.im/network", "3", nullptr, nullptr);
	conn_close(c.c);
	assert_true(conn_view_is_closed(w));
	conn_view_free(v);
	conn_view_free(q);
	conn_view_free(w);
	conn_close(b.c);
	peer_free(&c);
	peer_free(&b);
	finish_loop(&loop);
	log_free(&logger);
}

static void test_line_parsing(void **state) {
	(void)state;
	struct irc_msg *m;
	assert_true(conn_parse_line(STR_LIT(""), &m, nullptr));
	assert_null(m);
	assert_true(conn_parse_line(STR_LIT("PRIVMSG #a :x\r\nQUIT"), &m, nullptr));
	assert_msg_str(m, "PRIVMSG #a x");
	irc_msg_free(m);
	// C-only: NUL bytes become U+FFFD, invalid UTF-8 is decoded lossily
	assert_true(conn_parse_line(str_new("PRIVMSG #a :a\0b\xff", 16), &m, nullptr));
	assert_string_equal(m->params.data[1], "a\xEF\xBF\xBD"
					       "b\xEF\xBF\xBD");
	irc_msg_free(m);
}

// C-only: a line over MAX_LINE_LENGTH closes the connection; a line cut
// by EOF is dropped
static void test_line_limits(void **state) {
	(void)state;
	setup_log();
	uv_loop_t loop;
	uv_loop_init(&loop);
	int fds[2];
	socket_pair(fds);
	struct peer b;
	peer_init(&b, &loop, fds[1], nullptr);
	char *big = xmalloc(MAX_LINE_LENGTH + 10);
	memset(big, 'x', MAX_LINE_LENGTH + 10);
	assert_true(write(fds[0], "PING a\r\n", 8) == 8);
	assert_true(write(fds[0], big, MAX_LINE_LENGTH + 10) == (ssize_t)(MAX_LINE_LENGTH + 10));
	xfree(big);
	RUN_UNTIL(&loop, b.closed, 2000);
	assert_int_equal(b.got.len, 1);
	assert_string_equal(b.close_err, "line too long");
	close(fds[0]);
	peer_free(&b);

	socket_pair(fds);
	peer_init(&b, &loop, fds[1], nullptr);
	assert_true(write(fds[0], "PING a\nPING b", 13) == 13);
	close(fds[0]);
	RUN_UNTIL(&loop, b.closed, 2000);
	assert_int_equal(b.got.len, 1);
	assert_null(b.close_err);
	peer_free(&b);
	finish_loop(&loop);
	log_free(&logger);
}

T_MAIN(cmocka_unit_test(test_stream_roundtrip), cmocka_unit_test(test_rate_limited),
       cmocka_unit_test(test_queued_line), cmocka_unit_test(test_stream_wire_format),
       cmocka_unit_test(test_queue_budget), cmocka_unit_test(test_input_in_flight),
       cmocka_unit_test(test_view), cmocka_unit_test(test_line_parsing),
       cmocka_unit_test(test_line_limits))
