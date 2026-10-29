// Ported from soju-rs src/downstream/mod.rs (usernames, nickserv,
// network_attrs_roundtrip) and src/downstream/sasl.rs (sasl_mechanisms),
// plus C-only tests of registration over a connection, with the DB thread
// and the CPU-job limiter
#include "testsupport.h"

#include <sys/socket.h>
#include <unistd.h>

#include "base/alloc.h"
#include "db/db.h"
#include "downstream/downstream.h"

static void un_eq(const char *raw, const char *u, const char *c, const char *n) {
	char *gu, *gc, *gn;
	unmarshal_username(raw, &gu, &gc, &gn);
	assert_string_equal(gu, u);
	assert_string_equal(gc, c);
	assert_string_equal(gn, n);
	xfree(gu);
	xfree(gc);
	xfree(gn);
}

static void test_usernames(void **state) {
	(void)state;
	un_eq("alice", "alice", "", "");
	un_eq("alice/libera", "alice", "", "libera");
	un_eq("alice@phone", "alice", "phone", "");
	un_eq("alice/libera@phone", "alice", "phone", "libera");
	un_eq("alice@phone/libera", "alice", "phone", "libera");
}

static void ns_eq(const char *text, const char *acc, const char *pw) {
	char *a = nullptr, *p = nullptr;
	bool ok = parse_nickserv_credentials(text, "me", &a, &p);
	if (acc == nullptr) {
		assert_false(ok);
		return;
	}
	assert_true(ok);
	assert_string_equal(a, acc);
	assert_string_equal(p, pw);
	xfree(a);
	xfree(p);
}

static void test_nickserv(void **state) {
	(void)state;
	ns_eq("IDENTIFY pw", "me", "pw");
	ns_eq("identify acc pw", "acc", "pw");
	ns_eq("HELP", nullptr, nullptr);
	ns_eq("INFO x", nullptr, nullptr);
}

static void test_network_attrs_roundtrip(void **state) {
	(void)state;
	struct db_network n;
	db_network_init(&n, "irc+insecure://irc.example.org:6667");
	struct irc_tags attrs = IRC_TAGS_INIT;
	fill_network_addr_attrs(&attrs, &n);
	assert_string_equal(irc_tags_get(&attrs, "tls"), "0");
	assert_string_equal(irc_tags_get(&attrs, "host"), "irc.example.org");
	assert_string_equal(irc_tags_get(&attrs, "port"), "6667");
	irc_tags_free(&attrs);
	struct irc_tags upd = IRC_TAGS_INIT;
	irc_tags_parse(&upd, STR_LIT("tls=1;name=ex"));
	struct irc_msg *irc = nullptr;
	assert_true(update_network_attrs(&n, &upd, "CHANGENETWORK", &irc));
	assert_string_equal(n.addr, "irc.example.org:6667");
	assert_string_equal(n.name, "ex");
	irc_tags_free(&upd);
	irc_tags_parse(&upd, STR_LIT("bogus=1"));
	assert_false(update_network_attrs(&n, &upd, "X", &irc));
	assert_msg_str(irc, "FAIL BOUNCER UNKNOWN_ATTRIBUTE X bogus :Unknown attribute");
	irc_msg_free(irc);
	irc_tags_free(&upd);
	db_network_free(&n);
	db_network_init(&n, "");
	irc_tags_parse(&upd, STR_LIT("port=1"));
	assert_false(update_network_attrs(&n, &upd, "ADDNETWORK", &irc));
	irc_msg_free(irc);
	irc_tags_free(&upd);
	db_network_free(&n);
}

static void test_sasl_mechanisms(void **state) {
	(void)state;
	struct downstream_sasl *s = xcalloc(1, sizeof(*s));
	s->kind = SASL_PLAIN;
	s->mechanism = xstrdup("PLAIN");
	struct strbuf *ch;
	assert_true(downstream_sasl_next(s, nullptr, &ch, nullptr));
	assert_non_null(ch);
	assert_int_equal(ch->len, 0);
	strbuf_free(ch);
	xfree(ch);
	struct str r = str_new("\0alice\0pw", 9);
	assert_true(downstream_sasl_next(s, &r, &ch, nullptr));
	assert_null(ch);
	assert_string_equal(s->plain.username, "alice");
	r = STR_LIT("x");
	struct err *e = nullptr;
	assert_false(downstream_sasl_next(s, &r, &ch, &e));
	err_free(e);
	downstream_sasl_free(s);

	s = xcalloc(1, sizeof(*s));
	s->kind = SASL_OAUTHBEARER;
	s->mechanism = xstrdup("OAUTHBEARER");
	r = STR_LIT("n,a=alice,\x01"
		    "auth=Bearer tok\x01\x01");
	assert_true(downstream_sasl_next(s, &r, &ch, nullptr));
	assert_null(ch);
	assert_string_equal(s->oauth.username, "alice");
	assert_string_equal(s->oauth.token, "tok");
	downstream_sasl_free(s);
}

// Registration over a socket pair

struct env {
	struct loop_pool pool;
	struct db_thread *dbt;
	struct server srv;
	int client;
	struct strbuf in; // what the client received
	bool registered;
	char *username;
	struct downstream *dc;
};

static void on_registered(struct downstream *dc, void *data) {
	struct env *e = data;
	e->registered = true;
	e->username = xstrdup(dc->username);
	e->dc = dc;
}

static void env_setup(struct env *e) {
	*e = (struct env){0};
	loop_pool_init(&e->pool, 1);
	loop_set_current(&e->pool.loops[0]);
	struct db *db = nullptr;
	assert_true(db_open_in_memory(&db, nullptr));
	struct db_user u;
	db_user_init(&u, "alice");
	assert_true(db_user_set_password(&u, "secret", nullptr));
	assert_true(db_store_user(db, &u, nullptr));
	db_user_free(&u);
	assert_true(db_thread_start(&e->pool.loops[0].uv, db, 0, &e->dbt, nullptr));
	server_init(&e->srv, &e->pool, e->dbt);
	int fds[2];
	assert_int_equal(socketpair(AF_UNIX, SOCK_STREAM, 0, fds), 0);
	e->client = fds[1];
	uv_pipe_t *p = xcalloc(1, sizeof(*p));
	uv_pipe_init(&e->pool.loops[0].uv, p, 0);
	uv_pipe_open(p, fds[0]);
	struct logger log;
	log_init(&log, &e->srv.log_root);
	struct conn_options opts = {0};
	struct conn *c = conn_new((uv_stream_t *)p, &opts, &log, nullptr, nullptr);
	log_free(&log);
	struct conn_info info = {.remote_addr = (char *)(uintptr_t)"127.0.0.1:1234"};
	struct downstream_events ev = {.registered = on_registered, .data = e};
	downstream_serve(&e->srv, c, &info, &ev);
}

static void env_teardown(struct env *e) {
	if (e->dc != nullptr) {
		downstream_close(e->dc);
	}
	close(e->client);
	db_thread_stop(e->dbt);
	server_finish(&e->srv);
	loop_close_inbox(&e->pool.loops[0]);
	uv_run(&e->pool.loops[0].uv, UV_RUN_DEFAULT);
	loop_set_current(nullptr);
	loop_pool_free(&e->pool);
	strbuf_free(&e->in);
	xfree(e->username);
}

static void say(struct env *e, const char *line) {
	size_t n = strlen(line);
	assert_true(write(e->client, line, n) == (ssize_t)n);
}

// Runs the loop until the client received text containing want
static void expect(struct env *e, const char *want) {
	uint64_t deadline = uv_hrtime() + 10000000000ULL;
	while (strstr(strbuf_cstr(&e->in), want) == nullptr && uv_hrtime() < deadline) {
		uv_run(&e->pool.loops[0].uv, UV_RUN_NOWAIT);
		char buf[4096];
		ssize_t n = recv(e->client, buf, sizeof(buf), MSG_DONTWAIT);
		if (n > 0) {
			strbuf_append(&e->in, buf, (size_t)n);
		}
	}
	if (strstr(strbuf_cstr(&e->in), want) == nullptr) {
		fail_msg("expected %s in:\n%s", want, strbuf_cstr(&e->in));
	}
}

static void run_until_registered(struct env *e) {
	uint64_t deadline = uv_hrtime() + 10000000000ULL;
	while (!e->registered && uv_hrtime() < deadline) {
		uv_run(&e->pool.loops[0].uv, UV_RUN_NOWAIT);
	}
	assert_true(e->registered);
}

static void test_register_pass(void **state) {
	(void)state;
	struct env e;
	env_setup(&e);
	say(&e, "PASS secret\r\nNICK alice\r\nUSER alice/libera@phone 0 * :Alice\r\n");
	run_until_registered(&e);
	assert_string_equal(e.username, "alice");
	assert_string_equal(e.dc->client_name, "phone");
	assert_string_equal(e.dc->reg->network_name, "libera");
	env_teardown(&e);
}

static void test_register_wrong_password(void **state) {
	(void)state;
	struct env e;
	env_setup(&e);
	say(&e, "PASS wrong\r\nNICK alice\r\nUSER alice 0 * :Alice\r\n");
	expect(&e, ":localhost 464 * :Invalid credentials\r\n");
	assert_false(e.registered);
	env_teardown(&e);
}

static void test_register_sasl_plain(void **state) {
	(void)state;
	struct env e;
	env_setup(&e);
	say(&e, "CAP LS 302\r\nNICK alice\r\nUSER alice 0 * :Alice\r\n");
	expect(&e, "sasl=PLAIN");
	say(&e, "CAP REQ :sasl\r\n");
	expect(&e, ":localhost CAP * ACK sasl\r\n");
	say(&e, "AUTHENTICATE PLAIN\r\n");
	expect(&e, "AUTHENTICATE +\r\n");
	// "\0alice\0secret"
	say(&e, "AUTHENTICATE AGFsaWNlAHNlY3JldA==\r\n");
	expect(&e, " 903 * :SASL authentication successful\r\n");
	assert_false(e.registered);
	say(&e, "CAP END\r\n");
	run_until_registered(&e);
	assert_string_equal(e.username, "alice");
	env_teardown(&e);
}

static void test_register_sasl_failure(void **state) {
	(void)state;
	struct env e;
	env_setup(&e);
	say(&e, "CAP REQ :sasl\r\nAUTHENTICATE PLAIN\r\nAUTHENTICATE AGFsaWNlAHg=\r\n");
	expect(&e, " 904 * :Authentication failed\r\n");
	// Without credentials, a SASL client is told an account is required
	say(&e, "NICK alice\r\nUSER alice 0 * :Alice\r\nCAP END\r\n");
	expect(&e, "FAIL * ACCOUNT_REQUIRED :Authentication required\r\n");
	env_teardown(&e);
}

static void test_cap_labeled_and_errors(void **state) {
	(void)state;
	struct env e;
	env_setup(&e);
	say(&e, "CAP LS 302\r\nCAP REQ :labeled-response batch\r\n");
	expect(&e, "CAP * ACK :labeled-response batch\r\n");
	// A labeled command without a reply gets an ACK
	say(&e, "@label=a NICK bob\r\n");
	// soju-rs sends the ACK without a source (raw_send)
	expect(&e, "@label=a ACK\r\n");
	// Unknown commands, errors with the label
	say(&e, "@label=b FOO\r\n");
	expect(&e, "@label=b :localhost 421 * FOO :Unknown command\r\n");
	say(&e, "CAP REQ :unknown-cap\r\n");
	expect(&e, "CAP * NAK unknown-cap\r\n");
	say(&e, "CAP FOO\r\n");
	expect(&e, " 410 * FOO :Unknown CAP command\r\n");
	env_teardown(&e);
}

T_MAIN(cmocka_unit_test(test_usernames), cmocka_unit_test(test_nickserv),
       cmocka_unit_test(test_network_attrs_roundtrip), cmocka_unit_test(test_sasl_mechanisms),
       cmocka_unit_test(test_register_pass), cmocka_unit_test(test_register_wrong_password),
       cmocka_unit_test(test_register_sasl_plain), cmocka_unit_test(test_register_sasl_failure),
       cmocka_unit_test(test_cap_labeled_and_errors))
