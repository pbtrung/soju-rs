// C-only tests of Web Push through the server (soju-rs user/webpush.rs,
// downstream/cmds/webpush.rs): VAPID in ISUPPORT, WEBPUSH replies, and
// notifications for direct messages and read markers. Push services are
// external by definition: tests point subscriptions at internal addresses,
// which are refused, and observe the delivery failures in the log.
#include "server_fixture.h"

#include <pthread.h>
#include <unistd.h>

#include "base/alloc.h"
#include "base/timeutil.h"

static pthread_mutex_t log_mu = PTHREAD_MUTEX_INITIALIZER;
static unsigned push_failures;

static void sink(const char *line, void *data) {
	(void)data;
	if (strstr(line, "failed to send Web push notification to endpoint") != nullptr) {
		pthread_mutex_lock(&log_mu);
		push_failures++;
		pthread_mutex_unlock(&log_mu);
	}
}

static unsigned failures(void) {
	pthread_mutex_lock(&log_mu);
	unsigned n = push_failures;
	pthread_mutex_unlock(&log_mu);
	return n;
}

static bool wait_failures(unsigned n, unsigned ms) {
	for (unsigned i = 0; i < ms / 10 && failures() < n; i++) {
		usleep(10000);
	}
	return failures() >= n;
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
	atomic_store(&((struct db_task *)data)->done, true);
}
static void task_start(void *data) {
	struct db_task *t = data;
	server_db(t->srv, task_run, task_done, t);
}
static void on_db(struct server_fixture *e, db_job_fn run, void *arg) {
	struct db_task t = {.srv = &e->srv, .run = run, .arg = arg};
	loop_post(&e->loops.loops[0], task_start, nullptr, &t);
	for (unsigned i = 0; i < 2000 && !atomic_load(&t.done); i++) {
		usleep(5000);
	}
	assert_true(atomic_load(&t.done));
}

struct store {
	int64_t user_id, network_id;
	const char *endpoint, *vapid;
	size_t count; // subscriptions after the job
};
static void store_sub(struct db *db, void *data) {
	struct store *s = data;
	if (s->endpoint != nullptr) {
		struct db_webpush_subscription sub;
		db_webpush_subscription_init(&sub);
		xfree(sub.endpoint);
		xfree(sub.key_auth);
		xfree(sub.key_p256dh);
		xfree(sub.key_vapid);
		sub.endpoint = xstrdup(s->endpoint);
		sub.key_auth = xstrdup("AAAAAAAAAAAAAAAAAAAAAA");
		sub.key_p256dh = xstrdup("BAAA");
		sub.key_vapid = xstrdup(s->vapid);
		assert_true(db_store_webpush_subscription(db, s->user_id, s->network_id, &sub,
							  nullptr));
		db_webpush_subscription_free(&sub);
	}
	db_webpush_sub_vec subs = {0};
	assert_true(db_list_webpush_subscriptions(db, s->user_id, s->network_id, &subs, nullptr));
	s->count = subs.len;
	db_webpush_sub_vec_free(&subs);
}

// The next message with this command, its parameters joined by spaces
static char *reply(struct test_peer *p, const char *command) {
	struct irc_msg *m = peer_until(p, command);
	AUTO_STRBUF struct strbuf sb = STRBUF_INIT;
	for (size_t i = 0; i < m->params.len; i++) {
		strbuf_printf(&sb, "%s%s", i > 0 ? " " : "", m->params.data[i]);
	}
	irc_msg_free(m);
	return strbuf_take(&sb);
}

static void test_webpush_command(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, true);
	assert_non_null(e.srv.web_push);
	const char *vapid = e.srv.web_push->vapid_public;
	struct test_peer *plain = fixture_connect(&e);
	fixture_register(&e, plain, false, "");
	peer_send(plain, "WEBPUSH REGISTER https://example.org/x a=b");
	AUTOFREE char *r = reply(plain, "421");
	assert_string_equal(r, "* WEBPUSH Unknown command");

	struct test_peer *p = fixture_connect(&e);
	peer_send(p, "CAP LS 302");
	peer_send(p, "CAP REQ soju.im/webpush");
	peer_send(p, "PASS secret");
	peer_send(p, "NICK test");
	peer_send(p, "USER test 0 * :Test");
	peer_send(p, "CAP END");
	AUTOFREE char *want = xasprintf("VAPID=%s", vapid);
	struct irc_msg *m;
	bool found = false;
	while (!found) {
		m = peer_until(p, "005");
		for (size_t i = 0; i < m->params.len; i++) {
			found = found || cstr_eq(m->params.data[i], want);
		}
		irc_msg_free(m);
	}
	static const struct {
		const char *line, *command, *reply;
	} cases[] = {
		{"WEBPUSH", "461", "* WEBPUSH Not enough parameters"},
		{"WEBPUSH REGISTER https://example.org/x", "461",
		 "* WEBPUSH Not enough parameters"},
		{"WEBPUSH FOO", "FAIL", "WEBPUSH INVALID_PARAMS FOO Unknown command"},
		{"WEBPUSH REGISTER https://example.org/x auth=x", "FAIL",
		 "WEBPUSH INVALID_PARAMS REGISTER Missing auth or p256dh key"},
		{"WEBPUSH REGISTER http://example.org/x auth=x;p256dh=y", "FAIL",
		 "WEBPUSH INVALID_PARAMS REGISTER Invalid endpoint"},
		// The test notification can't be sent: never to internal addresses
		{"WEBPUSH REGISTER https://127.0.0.1:1/x auth=x;p256dh=y", "FAIL",
		 "WEBPUSH INVALID_PARAMS REGISTER Invalid endpoint"},
		{"WEBPUSH UNREGISTER https://example.org/none", "WEBPUSH",
		 "UNREGISTER https://example.org/none"},
	};
	for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
		peer_send(p, cases[i].line);
		AUTOFREE char *got = reply(p, cases[i].command);
		assert_string_equal(got, cases[i].reply);
	}
	// A known, fresh subscription with the same keys isn't checked again
	struct store s = {
		.user_id = e.user.id, .endpoint = "https://push.example/a", .vapid = vapid};
	on_db(&e, store_sub, &s);
	assert_int_equal(s.count, 1);
	peer_send(
		p,
		"WEBPUSH REGISTER https://push.example/a auth=AAAAAAAAAAAAAAAAAAAAAA;p256dh=BAAA");
	AUTOFREE char *ok = reply(p, "WEBPUSH");
	assert_string_equal(ok, "REGISTER https://push.example/a");
	peer_send(p, "WEBPUSH UNREGISTER https://push.example/a");
	AUTOFREE char *gone = reply(p, "WEBPUSH");
	assert_string_equal(gone, "UNREGISTER https://push.example/a");
	s.endpoint = nullptr;
	on_db(&e, store_sub, &s);
	assert_int_equal(s.count, 0);
	fixture_finish(&e);
}

static void test_webpush_notifications(void **state) {
	(void)state;
	log_set_sink(sink, nullptr);
	struct server_fixture e;
	fixture_init(&e, true);
	struct test_peer *up = fixture_upstream(&e, "", "");
	struct store s = {.user_id = e.user.id,
			  .network_id = e.network.id,
			  .endpoint = "https://127.0.0.1:1/push",
			  .vapid = e.srv.web_push->vapid_public};
	on_db(&e, store_sub, &s);
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, true, "draft/read-marker");
	// A direct message is pushed after 5 s, unless read meanwhile
	peer_send(up, ":Foo!u@h PRIVMSG test :hello");
	struct irc_msg *m = peer_until(dc, "PRIVMSG");
	irc_msg_free(m);
	assert_true(wait_failures(1, 15000));
	// Reading it dismisses the notification with another one
	AUTOFREE char *now = format_server_time_cstr(timestamp_now());
	AUTOFREE char *line = xasprintf("MARKREAD Foo timestamp=%s", now);
	peer_send(dc, line);
	m = peer_until(dc, "MARKREAD");
	irc_msg_free(m);
	assert_true(wait_failures(2, 15000));
	// A message from us isn't pushed, nor one read within 5 s
	peer_send(up, ":test!u@h PRIVMSG Foo :mine");
	peer_send(up, ":Foo!u@h PRIVMSG test :again");
	for (int i = 0; i < 2; i++) {
		m = peer_until(dc, "PRIVMSG");
		irc_msg_free(m);
	}
	AUTOFREE char *later = format_server_time_cstr(timestamp_now());
	AUTOFREE char *line2 = xasprintf("MARKREAD Foo timestamp=%s", later);
	peer_send(dc, line2);
	m = peer_until(dc, "MARKREAD");
	irc_msg_free(m);
	usleep(7000000);
	assert_int_equal(failures(), 2);
	fixture_finish(&e);
	log_set_sink(nullptr, nullptr);
}

T_MAIN(cmocka_unit_test(test_webpush_command), cmocka_unit_test(test_webpush_notifications))
