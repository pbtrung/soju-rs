// Ported from soju-rs src/auth.rs (internal, password_upgrade_kept, escape,
// http_auth), plus C-only tests of OAuth 2.0 discovery and introspection,
// PLAIN with a token, and SASL OAUTHBEARER end to end
#include "server_fixture.h"

#include <crypt.h>
#include <unistd.h>

#include "auth/auth.h"
#include "base/alloc.h"
#include "base/base64.h"
#include "http_mock.h"

static void wait_for(atomic_bool *flag) {
	uint64_t deadline = uv_hrtime() + 20000000000ULL;
	while (!atomic_load(flag) && uv_hrtime() < deadline) {
		usleep(1000);
	}
	assert_true(atomic_load(flag));
}

// An authentication run on loop 0, waited for by the test thread
struct auth_call {
	struct server *srv;
	const char *username, *password, *token, *remote_addr;
	atomic_bool done;
	char *internal, *external, *bearer_user;
};

static void record(struct auth_call *c, const struct auth_error *err) {
	if (err != nullptr) {
		c->internal = xstrdup(err->internal);
		c->external = xstrdup(err->external);
	}
	atomic_store(&c->done, true);
}

static void plain_done(void *data, const struct auth_error *err) {
	record(data, err);
}

static void bearer_done(void *data, const char *username, const struct auth_error *err) {
	struct auth_call *c = data;
	if (username != nullptr) {
		c->bearer_user = xstrdup(username);
	}
	record(c, err);
}

static void start(void *data) {
	struct auth_call *c = data;
	if (c->token != nullptr) {
		auth_oauth_bearer(c->srv, c->token, c->remote_addr, bearer_done, c);
	} else {
		auth_plain(c->srv, c->username, c->password, c->remote_addr, plain_done, c);
	}
}

static void call_free(struct auth_call *c) {
	xfree(c->internal);
	xfree(c->external);
	xfree(c->bearer_user);
}

static void auth(struct server_fixture *e, struct auth_call *c) {
	c->srv = &e->srv;
	if (c->remote_addr == nullptr) {
		c->remote_addr = "";
	}
	loop_post(&e->loops.loops[0], start, nullptr, c);
	wait_for(&c->done);
}

// Asserts the outcome of a PLAIN authentication: external is nullptr for a
// success
static void expect(struct server_fixture *e, const char *username, const char *password,
		   const char *external) {
	struct auth_call c = {.username = username, .password = password};
	auth(e, &c);
	assert_cstr_eq(c.external, external);
	call_free(&c);
}

static void test_internal(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, true);
	expect(&e, "test", "secret", nullptr);
	expect(&e, "test", "nope", "Invalid credentials");
	expect(&e, "bob", "secret", "Invalid credentials");
	fixture_finish(&e);
}

// A job on the DB thread, started from loop 0
struct db_task {
	struct server *srv;
	db_job_fn run;
	void *arg;
	atomic_bool done;
};
static void task_run(struct db *db, void *data) {
	struct db_task *t = data;
	t->run(db, t->arg);
}
static void task_done(void *data) {
	struct db_task *t = data;
	atomic_store(&t->done, true);
}
static void task_start(void *data) {
	struct db_task *t = data;
	server_db(t->srv, task_run, task_done, t);
}
static void on_db(struct server_fixture *e, db_job_fn run, void *arg) {
	struct db_task t = {.srv = &e->srv, .run = run, .arg = arg};
	loop_post(&e->loops.loops[0], task_start, nullptr, &t);
	wait_for(&t.done);
}

struct alice {
	struct db_user u;
	struct db_user stored;
};
static void store_alice(struct db *db, void *data) {
	struct alice *a = data;
	db_user_init(&a->u, "alice");
	char salt[CRYPT_GENSALT_OUTPUT_SIZE];
	assert_non_null(crypt_gensalt_rn("$2b$", 4, nullptr, 0, salt, sizeof(salt)));
	struct crypt_data *cd = xcalloc(1, sizeof(*cd));
	xfree(a->u.password);
	a->u.password = xstrdup(crypt_rn("secret", salt, cd, (int)sizeof(*cd)));
	xfree(cd);
	assert_true(db_store_user(db, &a->u, nullptr));
}
static void get_alice(struct db *db, void *data) {
	struct alice *a = data;
	bool found;
	db_user_free(&a->stored);
	assert_true(db_get_user(db, "alice", &a->stored, &found, nullptr));
	assert_true(found);
}
// A live user's record still holds the old hash
static void update_alice(struct db *db, void *data) {
	struct alice *a = data;
	xfree(a->u.realname);
	a->u.realname = xstrdup("Alice");
	assert_true(db_store_user_keep_password(db, &a->u, nullptr));
	assert_true(db_set_user_downstream_interacted_at(db, a->u.id, timestamp_now(), nullptr));
}

static void test_password_upgrade_kept(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, true);
	struct alice a = {0};
	db_user_init(&a.stored, "");
	on_db(&e, store_alice, &a);
	expect(&e, "alice", "secret", nullptr);
	on_db(&e, get_alice, &a);
	AUTOFREE char *upgraded = xstrdup(a.stored.password);
	assert_string_not_equal(upgraded, a.u.password);
	on_db(&e, update_alice, &a);
	on_db(&e, get_alice, &a);
	assert_string_equal(a.stored.password, upgraded);
	assert_string_equal(a.stored.realname, "Alice");
	expect(&e, "alice", "secret", nullptr);
	db_user_free(&a.u);
	db_user_free(&a.stored);
	fixture_finish(&e);
}

static void test_escape(void **state) {
	(void)state;
	AUTOFREE char *a = query_escape(STR_LIT("a b/c+d"));
	assert_string_equal(a, "a+b%2Fc%2Bd");
	AUTOFREE char *b = percent_decode(STR_LIT("a%20b"));
	assert_string_equal(b, "a b");
	AUTOFREE char *c = percent_decode(STR_LIT("%3a%3A"));
	assert_string_equal(c, "::");
	AUTOFREE char *d = percent_decode(STR_LIT("a%+fb%2"));
	assert_string_equal(d, "a%+fb%2");
}

struct mock_state {
	uint16_t port;
	atomic_bool forwarded; // X-Forwarded-For seen
	atomic_bool client_auth; // the introspection client credentials seen
};

static void http_auth_handler(const struct strbuf *req, struct strbuf *resp, void *data) {
	struct mock_state *m = data;
	AUTOFREE char *creds = base64_encode_cstr(BASE64_STANDARD, "alice:pw", 8);
	AUTOFREE char *want = xasprintf("Basic %s", creds);
	if (strstr(req->buf, "X-Forwarded-For: 1.2.3.4\r\n") != nullptr &&
	    strstr(req->buf, "Forwarded: for=\"1.2.3.4:5678\"\r\n") != nullptr) {
		atomic_store(&m->forwarded, true);
	}
	const char *status = strstr(req->buf, want) != nullptr ? "200 OK" : "403 Forbidden";
	strbuf_printf(resp, "HTTP/1.1 %s\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
		      status);
}

static void set_plain(struct server_fixture *e, enum plain_auth kind, const char *url,
		      struct oauth2 *o) {
	struct server_config *cfg = server_config_default();
	cfg->plain_auth = kind;
	xfree(cfg->http_auth_url);
	cfg->http_auth_url = xstrdup(url);
	cfg->oauth2 = o;
	server_config_set(&e->srv, cfg);
}

static void test_http_auth(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, true);
	struct mock_state ms = {0};
	struct http_mock *m = http_mock_start(http_auth_handler, &ms);
	AUTOFREE char *url = xasprintf("http://127.0.0.1:%u/auth", (unsigned)http_mock_port(m));
	set_plain(&e, PLAIN_AUTH_HTTP, url, nullptr);
	struct auth_call c = {.username = "alice", .password = "pw", .remote_addr = "1.2.3.4:5678"};
	auth(&e, &c);
	assert_null(c.external);
	call_free(&c);
	assert_true(atomic_load(&ms.forwarded));
	c = (struct auth_call){
		.username = "alice", .password = "bad", .remote_addr = "1.2.3.4:5678"};
	auth(&e, &c);
	assert_string_equal(c.external, "Invalid credentials");
	assert_string_equal(c.internal, "HTTP auth server returned forbidden");
	call_free(&c);
	http_mock_stop(m);
	// The server is gone: not the user's fault
	expect(&e, "alice", "pw", "Authentication failed");
	fixture_finish(&e);
}

static void oauth_handler(const struct strbuf *req, struct strbuf *resp, void *data) {
	struct mock_state *m = data;
	AUTO_STRBUF struct strbuf body = STRBUF_INIT;
	int status = 200;
	if (strncmp(req->buf, "GET /.well-known/oauth-authorization-server/auth ", 49) == 0) {
		strbuf_printf(&body,
			      "{\"issuer\":\"x\",\"introspection_endpoint\":"
			      "\"http://127.0.0.1:%u/introspect\","
			      "\"introspection_endpoint_auth_methods_supported\":"
			      "[\"client_secret_basic\"]}",
			      (unsigned)m->port);
	} else if (strncmp(req->buf, "GET /.well-known/oauth-authorization-server/none ", 49) ==
		   0) {
		strbuf_append_cstr(&body,
				   "{\"issuer\":\"x\",\"introspection_endpoint\":\"http://a/\","
				   "\"introspection_endpoint_auth_methods_supported\":"
				   "[\"none\"]}");
	} else if (strncmp(req->buf, "POST /introspect ", 17) == 0) {
		// "se/cret" escaped again for Basic credentials
		AUTOFREE char *b64 = base64_encode_cstr(BASE64_STANDARD, "id:se%2Fcret", 12);
		if (strstr(req->buf, b64) != nullptr) {
			atomic_store(&m->client_auth, true);
		}
		const char *token = strstr(req->buf, "\r\n\r\n") + 4;
		if (strcmp(token, "token=good%2Btoken") == 0) {
			strbuf_append_cstr(&body, "{\"active\":true,\"username\":\"test\"}");
		} else if (strcmp(token, "token=bob") == 0) {
			strbuf_append_cstr(&body, "{\"active\":true,\"username\":\"bob\"}");
		} else if (strcmp(token, "token=nouser") == 0) {
			strbuf_append_cstr(&body, "{\"active\":true}");
		} else {
			strbuf_append_cstr(&body, "{\"active\":false}");
		}
	} else {
		status = 404;
	}
	strbuf_printf(resp, "HTTP/1.1 %d X\r\nContent-Length: %zu\r\n\r\n%s", status, body.len,
		      strbuf_cstr(&body));
}

struct load_call {
	struct config cfg;
	struct auth_settings *settings;
	char *err;
	atomic_bool done;
};
static void loaded(void *data, struct auth_settings *s, const struct err *err) {
	struct load_call *c = data;
	c->settings = s;
	c->err = err != nullptr ? xstrdup(err_msg(err)) : nullptr;
	atomic_store(&c->done, true);
}
static void load_start(void *data) {
	struct load_call *c = data;
	auth_settings_load(&loop_current()->uv, &c->cfg, loaded, c);
}
struct directive {
	enum auth_driver driver;
	const char *source;
};

// Loads auth directives on loop 0
static void load(struct server_fixture *e, struct load_call *c, size_t n,
		 const struct directive *auths) {
	*c = (struct load_call){0};
	config_defaults(&c->cfg);
	for (size_t i = 0; i < c->cfg.auth.len; i++) {
		xfree(c->cfg.auth.data[i].source);
	}
	c->cfg.auth.len = 0;
	for (size_t i = 0; i < n; i++) {
		vec_push(&c->cfg.auth,
			 ((struct config_auth){auths[i].driver, xstrdup(auths[i].source)}));
	}
	loop_post(&e->loops.loops[0], load_start, nullptr, c);
	wait_for(&c->done);
	config_free(&c->cfg);
}

static void test_oauth2(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, true);
	struct mock_state ms = {0};
	struct http_mock *m = http_mock_start(oauth_handler, &ms);
	ms.port = http_mock_port(m);
	AUTOFREE char *url =
		xasprintf("http://id:se%%2Fcret@127.0.0.1:%u/auth/", (unsigned)ms.port);
	struct load_call lc;
	load(&e, &lc, 1, (struct directive[]){{AUTH_OAUTH2, url}});
	assert_null(lc.err);
	assert_int_equal(lc.settings->plain, PLAIN_AUTH_OAUTH2);
	assert_string_equal(lc.settings->oauth2->client_id, "id");
	assert_string_equal(lc.settings->oauth2->client_secret, "se/cret");
	oauth2_ref(lc.settings->oauth2);
	set_plain(&e, PLAIN_AUTH_OAUTH2, "", lc.settings->oauth2);
	auth_settings_free(lc.settings);

	struct auth_call c = {.token = "good+token"};
	auth(&e, &c);
	assert_string_equal(c.bearer_user, "test");
	assert_true(atomic_load(&ms.client_auth));
	call_free(&c);
	c = (struct auth_call){.token = "expired"};
	auth(&e, &c);
	assert_string_equal(c.external, "Invalid credentials");
	assert_string_equal(c.internal, "invalid access token");
	call_free(&c);
	c = (struct auth_call){.token = "nouser"};
	auth(&e, &c);
	assert_string_equal(c.external, "Authentication failed");
	call_free(&c);
	// PLAIN with a token as password: it must be the user's
	expect(&e, "test", "good+token", nullptr);
	expect(&e, "test", "bob", "Invalid credentials");

	// SASL OAUTHBEARER (RFC 7628) from a client
	struct test_peer *p = fixture_connect(&e);
	peer_send(p, "CAP LS 302");
	struct irc_msg *msg = peer_until(p, "CAP");
	assert_non_null(strstr(irc_msg_trailing(msg), "sasl=PLAIN,OAUTHBEARER"));
	irc_msg_free(msg);
	peer_send(p, "CAP REQ sasl");
	msg = peer_until(p, "CAP");
	irc_msg_free(msg);
	peer_send(p, "AUTHENTICATE OAUTHBEARER");
	msg = peer_until(p, "AUTHENTICATE");
	irc_msg_free(msg);
	static const char gs2[] = "n,a=test,\001auth=Bearer good+token\001\001";
	AUTOFREE char *b64 = base64_encode_cstr(BASE64_STANDARD, gs2, sizeof(gs2) - 1);
	AUTOFREE char *line = xasprintf("AUTHENTICATE %s", b64);
	peer_send(p, line);
	msg = peer_until(p, "903");
	irc_msg_free(msg);
	peer_send(p, "NICK test");
	peer_send(p, "USER test 0 * :Test");
	peer_send(p, "CAP END");
	msg = peer_until(p, "001");
	irc_msg_free(msg);
	http_mock_stop(m);
	fixture_finish(&e);
}

static void test_auth_settings_errors(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, true);
	struct mock_state ms = {0};
	struct http_mock *m = http_mock_start(oauth_handler, &ms);
	ms.port = http_mock_port(m);
	AUTOFREE char *missing = xasprintf("http://127.0.0.1:%u/missing", (unsigned)ms.port);
	AUTOFREE char *none = xasprintf("http://id@127.0.0.1:%u/none", (unsigned)ms.port);
	static const struct {
		struct directive a[2];
		size_t n;
		const char *err;
	} cases[] = {
		{{{AUTH_PAM, ""}},
		 1,
		 "failed to create authenticator pam: PAM support is disabled"},
		{{{AUTH_INTERNAL, ""}, {AUTH_HTTP, "http://x/"}},
		 2,
		 "failed to load authenticators: multiple plain authentication methods specified"},
		{{{AUTH_OAUTH2, nullptr}},
		 1,
		 "OAuth 2.0 discovery failed: OAuth 2.0 HTTP error: 404 Not Found"},
		{{{AUTH_OAUTH2, nullptr}},
		 1,
		 "OAuth 2.0 server doesn't support Basic HTTP authentication for introspection"},
	};
	for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
		struct directive a[2] = {cases[i].a[0], cases[i].a[1]};
		if (a[0].driver == AUTH_OAUTH2) {
			a[0].source = i == 2 ? missing : none;
		}
		struct load_call lc;
		load(&e, &lc, cases[i].n, a);
		assert_null(lc.settings);
		assert_cstr_eq(lc.err, cases[i].err);
		xfree(lc.err);
	}
	// Internal and HTTP plain settings need no discovery
	struct load_call lc;
	load(&e, &lc, 1, (struct directive[]){{AUTH_HTTP, "http://auth/"}});
	assert_int_equal(lc.settings->plain, PLAIN_AUTH_HTTP);
	assert_string_equal(lc.settings->http_url, "http://auth/");
	assert_null(lc.settings->oauth2);
	auth_settings_free(lc.settings);
	http_mock_stop(m);
	fixture_finish(&e);
}

T_MAIN(cmocka_unit_test(test_internal), cmocka_unit_test(test_password_upgrade_kept),
       cmocka_unit_test(test_escape), cmocka_unit_test(test_http_auth),
       cmocka_unit_test(test_oauth2), cmocka_unit_test(test_auth_settings_errors))
