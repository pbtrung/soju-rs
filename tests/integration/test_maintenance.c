#include "server_fixture.h"
#include "base/alloc.h"
#include "server/maintenance.h"

struct maintenance_test {
	struct server_fixture *fixture;
	struct maintenance *maintenance;
	uv_sem_t ready;
	size_t old_rows, remaining;
	bool enabled;
	double free_ratio;
};
static void start(void *data) {
	struct maintenance_test *test = data;
	test->maintenance = maintenance_start(&test->fixture->srv);
	uv_sem_post(&test->ready);
}
static void stop_maintenance(void *data) {
	struct maintenance_test *test = data;
	maintenance_stop(test->maintenance);
	uv_sem_post(&test->ready);
}
static void seed(struct db *db, void *data) {
	struct maintenance_test *test = data;
	struct server_fixture *fixture = test->fixture;
	struct irc_msg *old = IRC_MSG("PRIVMSG", "#test", "old");
	irc_tags_set(&old->tags, "time", "2020-01-01T00:00:00.000Z");
	irc_msg_vec messages = {0};
	for (size_t i = 0; i < test->old_rows; i++) {
		vec_push(&messages, old);
	}
	i64vec ids = {0};
	assert_true(db_store_messages(db, fixture->network.id, "#test", messages.data, messages.len,
				      &ids, nullptr));
	vec_free(&messages);
	vec_free(&ids);
	irc_msg_free(old);
	struct db_user record;
	bool found;
	assert_true(db_get_user(db, "test", &record, &found, nullptr));
	assert_true(found);
	record.downstream_interacted_at =
		(struct db_time){.set = true, .t = timestamp_from_unix(1577836800, 0)};
	assert_true(db_store_user_keep_password(db, &record, nullptr));
	db_user_free(&record);
}
static void inspect(struct db *db, void *data) {
	struct maintenance_test *test = data;
	int64_t last = 0;
	assert_true(db_get_message_last_id(db, test->fixture->network.id, "#test", &last, nullptr));
	test->remaining = (size_t)last;
	struct db_user record;
	bool found;
	assert_true(db_get_user(db, "test", &record, &found, nullptr));
	assert_true(found);
	test->enabled = record.enabled;
	db_user_free(&record);
	assert_true(db_free_page_ratio(db, &test->free_ratio, nullptr));
}
static void startup(void *data) {
	struct maintenance_test *test = data;
	maintenance_startup(&test->fixture->srv);
	uv_sem_post(&test->ready);
}
static void test_chunked_expiry_and_inactive_users(void **state) {
	(void)state;
	struct server_fixture fixture;
	fixture_init(&fixture, false);
	struct maintenance_test test = {.fixture = &fixture, .old_rows = 2001};
	db_thread_call(fixture.db, seed, &test);
	struct server_config *cfg = server_config_default();
	cfg->cfg.basic.message_expiry = 86400000000000ULL;
	cfg->cfg.basic.disable_inactive_users_delay = 86400000000000ULL;
	server_config_set(&fixture.srv, cfg);
	assert_int_equal(uv_sem_init(&test.ready, 0), 0);
	loop_post(&fixture.loops.loops[0], start, nullptr, &test);
	uv_sem_wait(&test.ready);
	uint64_t deadline = uv_hrtime() + 30000000000ULL;
	do {
		db_thread_call(fixture.db, inspect, &test);
		assert_true(uv_hrtime() < deadline);
		uv_sleep(10);
	} while (test.remaining != 0 || test.enabled);
	loop_post(&fixture.loops.loops[0], stop_maintenance, nullptr, &test);
	uv_sem_wait(&test.ready);
	uv_sem_destroy(&test.ready);
	fixture_finish(&fixture);
}
// Startup expiry deletes everything expired at once, then vacuums: no
// page freed by the deletion is left
static void test_startup_expiry_vacuums(void **state) {
	(void)state;
	struct server_fixture fixture;
	fixture_init(&fixture, false);
	struct maintenance_test test = {.fixture = &fixture, .old_rows = 4500};
	db_thread_call(fixture.db, seed, &test);
	struct server_config *cfg = server_config_default();
	cfg->cfg.basic.message_expiry = 86400000000000ULL;
	server_config_set(&fixture.srv, cfg);
	assert_int_equal(uv_sem_init(&test.ready, 0), 0);
	loop_post(&fixture.loops.loops[0], startup, nullptr, &test);
	uv_sem_wait(&test.ready);
	uv_sem_destroy(&test.ready);
	db_thread_call(fixture.db, inspect, &test);
	assert_int_equal(test.remaining, 0);
	assert_true(test.free_ratio == 0.0);
	fixture_finish(&fixture);
}
T_MAIN(cmocka_unit_test(test_chunked_expiry_and_inactive_users),
       cmocka_unit_test(test_startup_expiry_vacuums))
