#include "server_fixture.h"

#include <arpa/inet.h>
#include <errno.h>
#include <poll.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <unistd.h>

#include "base/alloc.h"
#include "server/http.h"

static void pool_run(void *data) {
	loop_pool_run(data);
}
static void close_inbox(void *data) {
	loop_close_inbox(data);
}
static void finish(void *data) {
	struct server_fixture *e = data;
	db_thread_stop(e->db);
	server_finish(&e->srv);
	for (unsigned i = 0; i < e->loops.n; i++) {
		loop_post(&e->loops.loops[i], close_inbox, nullptr, &e->loops.loops[i]);
	}
}

void fixture_init(struct server_fixture *e, bool memory) {
	*e = (struct server_fixture){.listener = -1};
	const char *loops = getenv("SOJU_LOOPS"), *delay = getenv("SOJU_DB_DELAY");
	unsigned long long count = 1, ms = 0;
	if (loops != nullptr) {
		assert_true(str_parse_u64(str_from(loops), &count));
	}
	if (delay != nullptr) {
		assert_true(str_parse_u64(str_from(delay), &ms));
	}
	loop_pool_init(&e->loops, (unsigned)count);
	struct db *db;
	assert_true(db_open_in_memory(&db, nullptr));
	db_user_init(&e->user, "test");
	assert_true(db_user_set_password(&e->user, "secret", nullptr));
	assert_true(db_store_user(db, &e->user, nullptr));
	e->listener = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
	if (e->listener < 0) {
		fail_msg("socket: %s", strerror(errno));
		return;
	}
	struct sockaddr_in addr = {.sin_family = AF_INET,
				   .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
	assert_int_equal(bind(e->listener, (struct sockaddr *)&addr, sizeof(addr)), 0);
	assert_int_equal(listen(e->listener, 16), 0);
	socklen_t len = sizeof(addr);
	assert_int_equal(getsockname(e->listener, (struct sockaddr *)&addr, &len), 0);
	AUTOFREE char *address =
		xasprintf("irc+insecure://127.0.0.1:%u", (unsigned)ntohs(addr.sin_port));
	db_network_init(&e->network, address);
	xfree(e->network.name);
	e->network.name = xstrdup("testnet");
	assert_true(db_store_network(db, e->user.id, &e->network, nullptr));
	assert_true(db_thread_start(&e->loops.loops[0].uv, db, (unsigned)ms, &e->db, nullptr));
	server_init(&e->srv, &e->loops, e->db);
	struct server_config *cfg = server_config_default();
	cfg->cfg.basic.msg_store_driver = memory ? MSG_STORE_MEMORY : MSG_STORE_DB;
	server_config_set(&e->srv, cfg);
	assert_true(server_start(&e->srv, nullptr));
	assert_int_equal(uv_thread_create(&e->runner, pool_run, &e->loops), 0);
}

void fixture_finish(struct server_fixture *e) {
	for (size_t i = 0; i < e->peers.len; i++) {
		if (e->peers.data[i]->fd >= 0) {
			close(e->peers.data[i]->fd);
		}
		strbuf_free(&e->peers.data[i]->pending);
		xfree(e->peers.data[i]);
	}
	vec_free(&e->peers);
	if (e->listener >= 0) {
		close(e->listener);
	}
	server_stop(&e->srv);
	uint64_t deadline = uv_hrtime() + 10000000000ULL;
	while (!server_is_idle(&e->srv) && uv_hrtime() < deadline) {
		usleep(1000);
	}
	assert_true(server_is_idle(&e->srv));
	loop_post(&e->loops.loops[0], finish, nullptr, e);
	uv_thread_join(&e->runner);
	loop_pool_free(&e->loops);
	db_user_free(&e->user);
	db_network_free(&e->network);
}

static struct test_peer *new_peer(struct server_fixture *e, int fd) {
	struct test_peer *p = xcalloc(1, sizeof(*p));
	*p = (struct test_peer){.fd = fd, .reply_ping = true};
	vec_push(&e->peers, p);
	return p;
}

void peer_send(struct test_peer *p, const char *line) {
	AUTO_STRBUF struct strbuf data = STRBUF_INIT;
	strbuf_append_cstr(&data, line);
	strbuf_append_cstr(&data, "\r\n");
	size_t written = 0;
	while (written < data.len) {
		ssize_t n = send(p->fd, data.buf + written, data.len - written, MSG_NOSIGNAL);
		if (n <= 0) {
			fail_msg("send: %s", strerror(errno));
			return;
		}
		written += (size_t)n;
	}
}
void peer_send_take(struct test_peer *p, struct irc_msg *msg) {
	AUTOFREE char *line = irc_msg_to_string(msg);
	peer_send(p, line);
	irc_msg_free(msg);
}

struct irc_msg *peer_read(struct test_peer *p) {
	uint64_t deadline = uv_hrtime() + 10000000000ULL;
	while (uv_hrtime() < deadline) {
		char *nl =
			p->pending.len > 0 ? memchr(p->pending.buf, '\n', p->pending.len) : nullptr;
		if (nl != nullptr) {
			size_t n = (size_t)(nl - p->pending.buf) + 1;
			struct irc_msg *msg = irc_msg_parse(str_new(p->pending.buf, n), nullptr);
			memmove(p->pending.buf, p->pending.buf + n, p->pending.len - n);
			strbuf_truncate(&p->pending, p->pending.len - n);
			assert_non_null(msg);
			if (p->reply_ping && cstr_eq(msg->command, "PING")) {
				peer_send_take(p, IRC_MSG("PONG", irc_msg_trailing(msg)));
				irc_msg_free(msg);
				continue;
			}
			return msg;
		}
		struct pollfd fd = {.fd = p->fd, .events = POLLIN};
		int ready = poll(&fd, 1, 100);
		if (ready < 0 && errno == EINTR) {
			continue;
		}
		assert_true(ready >= 0);
		if (ready == 0) {
			continue;
		}
		char buf[8192];
		ssize_t n = recv(p->fd, buf, sizeof(buf), 0);
		if (n <= 0) {
			fail_msg("connection closed while waiting for a line");
			return nullptr;
		}
		strbuf_append(&p->pending, buf, (size_t)n);
	}
	fail_msg("timed out reading IRC line; pending: %s", strbuf_cstr(&p->pending));
	return nullptr;
}

struct irc_msg *peer_until(struct test_peer *p, const char *command) {
	for (size_t i = 0; i < 1000; i++) {
		struct irc_msg *msg = peer_read(p);
		if (cstr_eq(msg->command, command)) {
			return msg;
		}
		irc_msg_free(msg);
	}
	fail_msg("too many messages waiting for %s", command);
	return nullptr;
}

void peer_roundtrip(struct test_peer *p, irc_msg_vec *out) {
	peer_send(p, "PING roundtrip-token");
	for (;;) {
		struct irc_msg *msg = peer_read(p);
		if (cstr_eq(msg->command, "PONG") &&
		    cstr_eq(irc_msg_trailing(msg), "roundtrip-token")) {
			irc_msg_free(msg);
			break;
		}
		if (!cstr_eq(msg->command, "BATCH") && !cstr_eq(msg->command, "CAP")) {
			vec_push(out, msg);
		} else {
			irc_msg_free(msg);
		}
	}
}
void peer_sync(struct test_peer *p) {
	irc_msg_vec messages = {0};
	peer_roundtrip(p, &messages);
	irc_msg_vec_free(&messages);
}

struct test_peer *fixture_upstream(struct server_fixture *e, const char *caps,
				   const char *isupport) {
	struct pollfd fd = {.fd = e->listener, .events = POLLIN};
	assert_int_equal(poll(&fd, 1, 10000), 1);
	int client = accept(e->listener, nullptr, nullptr);
	if (client < 0) {
		fail_msg("accept failed");
		return nullptr;
	}
	struct test_peer *p = new_peer(e, client);
	struct irc_msg *m = peer_until(p, "USER");
	irc_msg_free(m);
	peer_send_take(p, IRC_MSG("CAP", "*", "LS", caps));
	m = peer_until(p, "CAP");
	if (cstr_eq(irc_msg_param(m, 0), "REQ")) {
		peer_send_take(p, IRC_MSG("CAP", "*", "ACK", irc_msg_param(m, 1)));
		irc_msg_free(m);
		m = peer_until(p, "CAP");
	}
	assert_string_equal(irc_msg_param(m, 0), "END");
	irc_msg_free(m);
	peer_send(p, ":server 001 test :Welcome");
	AUTO_STRBUF struct strbuf support = STRBUF_INIT;
	strbuf_printf(&support,
		      ":server 005 test soju.im/SAFERATE CASEMAPPING=rfc1459 %s :supported",
		      isupport);
	peer_send(p, strbuf_cstr(&support));
	peer_send(p, ":server 422 test :No MOTD");
	peer_sync(p);
	return p;
}

struct accepted {
	struct server *srv;
	int fd;
};
static void accept_client(void *data) {
	struct accepted *ev = data;
	uv_pipe_t *pipe = xcalloc(1, sizeof(*pipe));
	uv_pipe_init(&loop_current()->uv, pipe, 0);
	uv_pipe_open(pipe, ev->fd);
	struct conn *conn = conn_new((uv_stream_t *)pipe, &(struct conn_options){0}, &ev->srv->log,
				     nullptr, nullptr);
	downstream_serve(ev->srv, conn,
			 &(struct conn_info){.remote_addr = (char *)(uintptr_t)"127.0.0.1:1234"},
			 nullptr);
	xfree(ev);
}
struct http_accepted {
	struct server *srv;
	int fd;
	bool prometheus;
};
static void accept_http(void *data) {
	struct http_accepted *ev = data;
	uv_pipe_t *pipe = xcalloc(1, sizeof(*pipe));
	uv_pipe_init(&loop_current()->uv, pipe, 0);
	uv_pipe_open(pipe, ev->fd);
	struct conn *conn =
		conn_new((uv_stream_t *)pipe, &(struct conn_options){.framing = CONN_FRAMING_RAW},
			 &ev->srv->log, nullptr, nullptr);
	struct conn_info info = {.remote_addr = (char *)(uintptr_t)"127.0.0.1:1234",
				 .has_remote_ip = true};
	assert_true(ipaddr_parse(STR_LIT("127.0.0.1"), &info.remote_ip));
	http_serve(ev->srv, conn, &info, ev->prometheus);
	xfree(ev);
}
int fixture_connect_http(struct server_fixture *e, bool prometheus) {
	int fds[2];
	assert_int_equal(socketpair(AF_UNIX, SOCK_STREAM, 0, fds), 0);
	struct http_accepted *ev = xmalloc(sizeof(*ev));
	*ev = (struct http_accepted){.srv = &e->srv, .fd = fds[0], .prometheus = prometheus};
	loop_post(&e->loops.loops[e->next_conn++ % e->loops.n], accept_http, nullptr, ev);
	return fds[1];
}
struct test_peer *fixture_connect(struct server_fixture *e) {
	int fds[2];
	assert_int_equal(socketpair(AF_UNIX, SOCK_STREAM, 0, fds), 0);
	struct accepted *ev = xmalloc(sizeof(*ev));
	*ev = (struct accepted){.srv = &e->srv, .fd = fds[0]};
	loop_post(&e->loops.loops[e->next_conn++ % e->loops.n], accept_client, nullptr, ev);
	return new_peer(e, fds[1]);
}
void fixture_register(struct server_fixture *e, struct test_peer *p, bool network,
		      const char *caps) {
	(void)e;
	peer_send(p, "CAP LS 302");
	struct irc_msg *m;
	for (;;) {
		m = peer_until(p, "CAP");
		bool last = m->params.len < 4;
		irc_msg_free(m);
		if (last) {
			break;
		}
	}
	if (caps[0] != '\0') {
		peer_send_take(p, IRC_MSG("CAP", "REQ", caps));
		m = peer_until(p, "CAP");
		assert_string_equal(irc_msg_param(m, 1), "ACK");
		irc_msg_free(m);
	}
	peer_send(p, "PASS secret");
	peer_send(p, "NICK test");
	peer_send(p, network ? "USER test/testnet 0 * :Test" : "USER test 0 * :Test");
	peer_send(p, "CAP END");
	m = peer_until(p, "001");
	irc_msg_free(m);
	peer_sync(p);
}

struct user_call {
	user_run_fn fn;
	void *data;
	atomic_bool done;
};
static void run_user(struct user *u, void *data) {
	struct user_call *call = data;
	call->fn(u, call->data);
	atomic_store(&call->done, true);
}
void fixture_run_user(struct server_fixture *e, user_run_fn fn, void *data) {
	struct user_call call = {.fn = fn, .data = data};
	user_post(&e->srv, e->user.id, run_user, nullptr, &call);
	uint64_t deadline = uv_hrtime() + 10000000000ULL;
	while (!atomic_load(&call.done) && uv_hrtime() < deadline) {
		usleep(1000);
	}
	assert_true(atomic_load(&call.done));
}
