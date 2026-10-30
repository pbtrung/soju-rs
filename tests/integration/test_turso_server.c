#include "server_fixture.h"
#include "turso_mock.h"
#include "base/alloc.h"
#include "service/service.h"
#include <unistd.h>

struct command {
	struct server *srv;
	const char *text;
	uv_sem_t done;
	struct strbuf output;
};
static void command_print(void *data, const char *line) {
	struct command *c = data;
	strbuf_append_cstr(&c->output, line);
}
static void command_done(void *data) {
	struct command *c = data;
	uv_sem_post(&c->done);
}
static void command_start(void *data) {
	struct command *c = data;
	service_global(c->srv, true, c->text, command_print, command_done, c);
}
static void gate(struct turso_mock *m, const char *sql) {
	pthread_mutex_lock(&m->mu);
	xfree(m->st.delay_before_sql);
	m->st.delay_before_sql = xstrdup(sql);
	m->st.hold_before_sql = true;
	m->st.before_sql_entered = false;
	pthread_mutex_unlock(&m->mu);
}
static void entered(struct turso_mock *m) {
	pthread_mutex_lock(&m->mu);
	while (!m->st.before_sql_entered) {
		pthread_cond_wait(&m->gate, &m->mu);
	}
	pthread_mutex_unlock(&m->mu);
}
static void release(struct turso_mock *m) {
	pthread_mutex_lock(&m->mu);
	m->st.hold_before_sql = false;
	pthread_cond_broadcast(&m->gate);
	pthread_mutex_unlock(&m->mu);
}
static void test_slow_storage_keeps_runtime_and_directory_responsive(void **state) {
	(void)state;
	struct turso_mock *m = turso_mock_start();
	struct server_fixture e;
	fixture_init_database(&e, turso_mock_database(m), false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	(void)up;
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, false, "");
	gate(m, "UPDATE User");
	struct command update = {.srv = &e.srv, .text = "user run test user update -nick updated"};
	assert_int_equal(uv_sem_init(&update.done, 0), 0);
	loop_post(&e.loops.loops[0], command_start, nullptr, &update);
	entered(m);
	// These operations must complete while the remote write is held. A
	// single-loop fixture catches accidental synchronous DB calls here.
	peer_sync(dc);
	int64_t id = 0;
	assert_true(server_find_user(&e.srv, "test", &id));
	assert_int_equal(id, e.user.id);
	release(m);
	uv_sem_wait(&update.done);
	assert_non_null(strstr(strbuf_cstr(&update.output), "updated user"));
	uv_sem_destroy(&update.done);
	strbuf_free(&update.output);

	gate(m, "CREATE TABLE Network");
	struct command create = {.srv = &e.srv,
				 .text = "user create -username bob -disable-password"};
	assert_int_equal(uv_sem_init(&create.done, 0), 0);
	loop_post(&e.loops.loops[0], command_start, nullptr, &create);
	entered(m);
	assert_true(server_find_user(&e.srv, "test", &id));
	peer_sync(dc);
	release(m);
	uv_sem_wait(&create.done);
	assert_non_null(strstr(strbuf_cstr(&create.output), "created user"));
	assert_true(server_find_user(&e.srv, "bob", &id));
	uv_sem_destroy(&create.done);
	strbuf_free(&create.output);
	fixture_finish(&e);
	turso_mock_stop(m);
}
struct failing {
	struct turso_mock *mock;
	struct db *db;
};
static void fail_user_database(struct server_fixture *e, void *data) {
	struct failing *f = data;
	char *name = turso_mock_linked_name(f->db, e->user.id);
	pthread_mutex_lock(&f->mock->mu);
	xfree(f->mock->st.fail_sql);
	f->mock->st.fail_sql = name;
	pthread_mutex_unlock(&f->mock->mu);
}

struct networks {
	uv_sem_t done;
	size_t count;
};
static void count_networks(struct user *u, void *data) {
	struct networks *n = data;
	n->count = u->networks.len;
	uv_sem_post(&n->done);
}

// A transient database failure while a user starts is retried: the user
// loads with all their networks once the database answers again
static void test_user_loading_retries_transient_failures(void **state) {
	(void)state;
	struct turso_mock *m = turso_mock_start();
	struct db *db = turso_mock_database(m);
	struct server_fixture e;
	struct failing f = {m, db};
	fixture_init_database_hook(&e, db, false, fail_user_database, &f);
	// The first load fails meanwhile
	usleep(300000);
	pthread_mutex_lock(&m->mu);
	xfree(m->st.fail_sql);
	m->st.fail_sql = nullptr;
	pthread_mutex_unlock(&m->mu);
	struct networks n = {0};
	uint64_t deadline = uv_hrtime() + 10000000000ULL;
	do {
		usleep(100000);
		assert_true(uv_hrtime() < deadline);
		assert_int_equal(uv_sem_init(&n.done, 0), 0);
		user_post(&e.srv, e.user.id, count_networks, nullptr, &n);
		uv_sem_wait(&n.done);
		uv_sem_destroy(&n.done);
	} while (n.count == 0);
	assert_int_equal(n.count, 1);
	fixture_finish(&e);
	turso_mock_stop(m);
}
T_MAIN(cmocka_unit_test(test_slow_storage_keeps_runtime_and_directory_responsive),
       cmocka_unit_test(test_user_loading_retries_transient_failures))
