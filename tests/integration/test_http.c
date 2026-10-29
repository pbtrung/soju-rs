// HTTP listeners end to end (soju-rs listen.rs serve_http): routes,
// keep-alive, Prometheus metrics, the WebSocket handshake checks and an IRC
// session over WebSocket. Ported from soju-rs src/metrics.rs (render).
#include "server_fixture.h"

#include <errno.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <unistd.h>

#include "base/alloc.h"
#include "http/websocket.h"
#include "server/metrics.h"

static void test_render(void **state) {
	(void)state;
	struct server_metrics m = {0};
	atomic_fetch_add(&m.worker_panics_total, 1);
	atomic_fetch_add(&m.downstreams, 2);
	AUTO_STRBUF struct strbuf s = STRBUF_INIT;
	metrics_render(&m, 3, &s);
	assert_non_null(strstr(strbuf_cstr(&s), "soju_users_active 3\n"));
	assert_non_null(strstr(strbuf_cstr(&s), "soju_downstreams_active 2\n"));
	assert_non_null(strstr(strbuf_cstr(&s), "soju_worker_panics_total 1\n"));
}

static void send_all(int fd, const char *p, size_t n) {
	while (n > 0) {
		ssize_t w = send(fd, p, n, MSG_NOSIGNAL);
		if (w <= 0) {
			fail_msg("send: %s", strerror(errno));
			return;
		}
		p += w;
		n -= (size_t)w;
	}
}

// Reads more bytes into buf; false at EOF
static bool read_more(int fd, struct strbuf *buf) {
	struct pollfd p = {.fd = fd, .events = POLLIN};
	if (poll(&p, 1, 10000) != 1) {
		fail_msg("timed out reading; got: %s", strbuf_cstr(buf));
		return false;
	}
	char tmp[8192];
	ssize_t n = recv(fd, tmp, sizeof(tmp), 0);
	if (n <= 0) {
		return false;
	}
	strbuf_append(buf, tmp, (size_t)n);
	return true;
}

struct response {
	int status;
	struct strbuf head; // status line and headers
	struct strbuf body;
};

static void response_free(struct response *r) {
	strbuf_free(&r->head);
	strbuf_free(&r->body);
}

// Reads a response from pending/fd; head_only for responses to HEAD
static void read_response(int fd, struct strbuf *pending, struct response *r, bool head_only) {
	*r = (struct response){0};
	ptrdiff_t end;
	while ((end = str_find(strbuf_str(pending), STR_LIT("\r\n\r\n"))) < 0) {
		if (!read_more(fd, pending)) {
			fail_msg("EOF before the response head");
		}
	}
	strbuf_append(&r->head, pending->buf, (size_t)end + 2);
	assert_true(str_starts_with(strbuf_str(&r->head), STR_LIT("HTTP/1.1 ")));
	r->status = (int)strtol(r->head.buf + strlen("HTTP/1.1 "), nullptr, 10);
	size_t len = 0;
	const char *cl = strstr(r->head.buf, "content-length: ");
	if (cl != nullptr) {
		len = strtoul(cl + strlen("content-length: "), nullptr, 10);
	}
	if (head_only) {
		len = 0;
	}
	size_t start = (size_t)end + 4;
	while (pending->len < start + len) {
		if (!read_more(fd, pending)) {
			fail_msg("EOF in the response body");
		}
	}
	strbuf_append(&r->body, pending->buf + start, len);
	strbuf_drop_front(pending, start + len);
}

static void expect_eof(int fd, struct strbuf *pending) {
	while (read_more(fd, pending)) {
	}
	assert_int_equal(pending->len, 0);
}

static void test_routes(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, true);
	char icon[] = "/tmp/soju-icon-XXXXXX";
	int ifd = mkstemp(icon);
	assert_true(ifd >= 0);
	assert_int_equal(write(ifd, "\x89PNG\0data", 9), 9);
	close(ifd);
	AUTOFREE char *icon_png = xasprintf("%s.png", icon);
	assert_int_equal(rename(icon, icon_png), 0);
	struct server_config *cfg = server_config_default();
	xfree(cfg->icon_path);
	cfg->icon_path = xstrdup(icon_png);
	server_config_set(&e.srv, cfg);

	int fd = fixture_connect_http(&e, false);
	AUTO_STRBUF struct strbuf pending = STRBUF_INIT;
	// Pipelined requests on a kept-alive connection, a body skipped
	const char *reqs = "GET / HTTP/1.1\r\nHost: x\r\n\r\n"
			   "POST /icon HTTP/1.1\r\nContent-Length: 3\r\n\r\nabc"
			   "HEAD /icon HTTP/1.1\r\n\r\n"
			   "GET /icon?v=1 HTTP/1.1\r\nConnection: close\r\n\r\n";
	send_all(fd, reqs, strlen(reqs));
	struct response r;
	read_response(fd, &pending, &r, false);
	assert_int_equal(r.status, 404);
	assert_sb_eq(&r.body, "404 page not found\n");
	response_free(&r);
	for (int i = 0; i < 3; i++) {
		read_response(fd, &pending, &r, i == 1);
		assert_int_equal(r.status, 200);
		assert_non_null(strstr(r.head.buf, "content-type: image/png\r\n"));
		assert_non_null(strstr(r.head.buf, "content-length: 9\r\n"));
		assert_int_equal(r.body.len, i == 1 ? 0 : 9);
		if (i != 1) {
			assert_memory_equal(r.body.buf, "\x89PNG\0data", 9);
		}
		response_free(&r);
	}
	expect_eof(fd, &pending);
	close(fd);

	// Malformed requests get a 400 and the connection is closed
	fd = fixture_connect_http(&e, false);
	send_all(fd, "GET /\r\n\r\n", 9);
	read_response(fd, &pending, &r, false);
	assert_int_equal(r.status, 400);
	response_free(&r);
	expect_eof(fd, &pending);
	close(fd);
	unlink(icon_png);
	fixture_finish(&e);
}

static void test_prometheus(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, true);
	int fd = fixture_connect_http(&e, true);
	AUTO_STRBUF struct strbuf pending = STRBUF_INIT;
	send_all(fd, "GET /anything HTTP/1.0\r\n\r\n", 26);
	struct response r;
	read_response(fd, &pending, &r, false);
	assert_int_equal(r.status, 200);
	assert_non_null(
		strstr(r.head.buf, "content-type: text/plain; version=0.0.4; charset=utf-8\r\n"));
	assert_non_null(strstr(r.body.buf, "# TYPE soju_users_active gauge\nsoju_users_active "));
	assert_non_null(strstr(r.body.buf, "soju_upstream_connect_errors_total 0\n"));
	response_free(&r);
	// HTTP/1.0 without keep-alive: closed after the response
	expect_eof(fd, &pending);
	close(fd);
	fixture_finish(&e);
}

static void expect_ws_error(struct server_fixture *e, const char *headers, int status,
			    const char *body) {
	int fd = fixture_connect_http(e, false);
	AUTO_STRBUF struct strbuf req = STRBUF_INIT;
	strbuf_printf(&req, "%s\r\n", headers);
	send_all(fd, req.buf, req.len);
	AUTO_STRBUF struct strbuf pending = STRBUF_INIT;
	struct response r;
	read_response(fd, &pending, &r, false);
	assert_int_equal(r.status, status);
	assert_sb_eq(&r.body, body);
	response_free(&r);
	close(fd);
}

static void test_websocket_handshake_errors(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, true);
	expect_ws_error(&e, "GET /socket HTTP/1.1\r\n", 426,
			"WebSocket protocol violation: Connection header \"\" does not contain "
			"Upgrade\n");
	expect_ws_error(
		&e, "GET /socket HTTP/1.1\r\nConnection: Upgrade\r\n", 426,
		"WebSocket protocol violation: Upgrade header does not contain websocket\n");
	expect_ws_error(&e,
			"POST /socket HTTP/1.1\r\nConnection: upgrade\r\nUpgrade: WebSocket\r\n",
			405, "WebSocket protocol violation: handshake request method is not GET\n");
	expect_ws_error(&e,
			"GET /socket HTTP/1.1\r\nConnection: upgrade\r\nUpgrade: websocket\r\n"
			"Sec-WebSocket-Version: 8\r\n",
			400, "unsupported WebSocket protocol version (only 13 is supported)\n");
	expect_ws_error(&e,
			"GET /socket HTTP/1.1\r\nConnection: upgrade\r\nUpgrade: websocket\r\n"
			"Sec-WebSocket-Version: 13\r\n",
			400, "WebSocket protocol violation: missing Sec-WebSocket-Key\n");
	expect_ws_error(&e,
			"GET /socket HTTP/1.1\r\nConnection: upgrade\r\nUpgrade: websocket\r\n"
			"Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: x\r\n"
			"Host: soju.example\r\nOrigin: https://evil.example\r\n",
			403,
			"request Origin \"evil.example\" is not authorized for Host "
			"\"soju.example\"\n");
	fixture_finish(&e);
}

struct ws_client {
	int fd;
	struct strbuf pending;
	struct ws_reader r;
};

static void ws_send(struct ws_client *c, const char *line) {
	AUTO_STRBUF struct strbuf f = STRBUF_INIT;
	ws_write_frame(&f, WS_OP_TEXT, line, strlen(line), (const uint8_t[]){1, 2, 3, 4});
	send_all(c->fd, f.buf, f.len);
}

// The next IRC message with this command
static struct irc_msg *ws_until(struct ws_client *c, const char *command) {
	for (;;) {
		size_t used;
		struct ws_event ev;
		enum ws_event_kind k = ws_read(&c->r, strbuf_str(&c->pending), &used, &ev);
		strbuf_drop_front(&c->pending, used);
		if (k == WS_NEED_MORE) {
			if (!read_more(c->fd, &c->pending)) {
				fail_msg("EOF waiting for %s", command);
				return nullptr;
			}
			continue;
		}
		assert_int_equal(k, WS_MESSAGE);
		assert_null(memchr(ev.payload.buf, '\n', ev.payload.len));
		struct irc_msg *m = irc_msg_parse(strbuf_str(&ev.payload), nullptr);
		ws_event_free(&ev);
		assert_non_null(m);
		if (cstr_eq(m->command, command)) {
			return m;
		}
		irc_msg_free(m);
	}
}

static void test_websocket_session(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, true);
	struct ws_client c = {.fd = fixture_connect_http(&e, false)};
	ws_reader_init(&c.r, false);
	// The first message is sent with the handshake
	AUTO_STRBUF struct strbuf req = STRBUF_INIT;
	strbuf_append_cstr(&req,
			   "GET /socket HTTP/1.1\r\nHost: soju.example\r\n"
			   "Origin: https://soju.example\r\n"
			   "Connection: keep-alive, Upgrade\r\nUpgrade: websocket\r\n"
			   "Sec-WebSocket-Version: 13\r\n"
			   "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
			   "Sec-WebSocket-Protocol: binary.ircv3.net, text.ircv3.net\r\n\r\n");
	ws_write_frame(&req, WS_OP_TEXT, "CAP LS 302", 10, (const uint8_t[]){7, 7, 7, 7});
	send_all(c.fd, req.buf, req.len);
	struct response r;
	read_response(c.fd, &c.pending, &r, false);
	assert_int_equal(r.status, 101);
	assert_non_null(
		strstr(r.head.buf, "sec-websocket-accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
	assert_non_null(strstr(r.head.buf, "sec-websocket-protocol: text.ircv3.net\r\n"));
	response_free(&r);
	struct irc_msg *m = ws_until(&c, "CAP");
	irc_msg_free(m);
	ws_send(&c, "PASS secret");
	ws_send(&c, "NICK test");
	ws_send(&c, "USER test 0 * :Test");
	ws_send(&c, "CAP END");
	m = ws_until(&c, "001");
	irc_msg_free(m);
	ws_send(&c, "PING token");
	m = ws_until(&c, "PONG");
	assert_string_equal(irc_msg_trailing(m), "token");
	irc_msg_free(m);
	// Closing: the server answers the close frame
	AUTO_STRBUF struct strbuf f = STRBUF_INIT;
	ws_write_close(&f, WS_CLOSE_NORMAL, (const uint8_t[]){1, 1, 1, 1});
	send_all(c.fd, f.buf, f.len);
	for (;;) {
		size_t used;
		struct ws_event ev;
		enum ws_event_kind k = ws_read(&c.r, strbuf_str(&c.pending), &used, &ev);
		strbuf_drop_front(&c.pending, used);
		ws_event_free(&ev);
		if (k == WS_CLOSE) {
			break;
		}
		if (k == WS_NEED_MORE && !read_more(c.fd, &c.pending)) {
			fail_msg("EOF before the close frame");
		}
	}
	close(c.fd);
	strbuf_free(&c.pending);
	ws_reader_finish(&c.r);
	fixture_finish(&e);
}

T_MAIN(cmocka_unit_test(test_render), cmocka_unit_test(test_routes),
       cmocka_unit_test(test_prometheus), cmocka_unit_test(test_websocket_handshake_errors),
       cmocka_unit_test(test_websocket_session))
