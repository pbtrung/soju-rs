// Ported from soju-rs src/database/connection.rs
// (stream_redirect_cannot_expose_the_group_token_to_an_untrusted_origin)
// and src/database/turso/http.rs (usage_counts_execute_and_batch_results_once),
// plus C-only tests of the Hrana client against the mock Turso server:
// values, transactions, faults, replication indexes and atomic batches
#include "testsupport.h"

#include <yyjson.h>

#include "base/alloc.h"
#include "db/conn.h"
#include "db/hrana.h"
#include "turso_mock.h"

static void
test_stream_redirect_cannot_expose_the_group_token_to_an_untrusted_origin(void **state) {
	(void)state;
	char *out = nullptr;
	assert_true(hrana_stream_endpoint("https://primary-example.turso.io/", &out, nullptr));
	assert_string_equal(out, "https://primary-example.turso.io/v2/pipeline");
	xfree(out);
	static const char *const bad[] = {
		"http://primary-example.turso.io/",
		"https://example.com/",
		"https://example.turso.io.evil.test/",
		"https://user:secret@primary-example.turso.io/",
	};
	for (size_t i = 0; i < sizeof(bad) / sizeof(bad[0]); i++) {
		assert_false(hrana_stream_endpoint(bad[i], &out, nullptr));
	}
}

static void test_usage_counts_execute_and_batch_results_once(void **state) {
	(void)state;
	struct hrana_shared *s = hrana_shared_new();
	static const char json[] =
		"{\"results\":["
		"{\"type\":\"ok\",\"response\":{\"type\":\"execute\",\"result\":{\"rows_read\":2,"
		"\"rows_written\":3}}},"
		"{\"type\":\"ok\",\"response\":{\"type\":\"batch\",\"result\":{\"rows_read\":999,"
		"\"step_results\":[{\"rows_read\":5,\"rows_written\":7},null,{\"rows_read\":11,"
		"\"rows_written\":13}]}}},"
		"{\"type\":\"error\",\"error\":{\"code\":\"SQLITE_ERROR\"}},"
		"{\"type\":\"ok\",\"response\":{\"type\":\"close\"}}]}";
	yyjson_doc *doc = yyjson_read(json, sizeof(json) - 1, 0);
	hrana_observe(s, doc);
	yyjson_doc_free(doc);
	uint64_t read, written;
	hrana_shared_usage(s, &read, &written);
	assert_int_equal(read, 18);
	assert_int_equal(written, 23);
	doc = yyjson_read("null", 4, 0);
	hrana_observe(s, doc);
	yyjson_doc_free(doc);
	hrana_shared_usage(s, &read, &written);
	assert_int_equal(read, 18);
	hrana_shared_unref(s);
}

// A remote connection to a new mock database
struct env {
	struct turso_mock *m;
	struct hrana_shared *shared;
	struct db_conn c;
};

static void env_init(struct env *e) {
	e->m = turso_mock_start();
	pthread_mutex_lock(&e->m->mu);
	strmap_set(&e->m->st.databases, "tenant", xstrdup("soju"));
	pthread_mutex_unlock(&e->m->mu);
	e->shared = hrana_shared_new();
	AUTOFREE char *url = xasprintf("%s/db/tenant/v2/pipeline", e->m->url);
	db_conn_init_remote(&e->c, hrana_new(url, "sql-secret", e->shared));
	assert_true(db_exec(&e->c, "CREATE TABLE t (id INTEGER PRIMARY KEY, a, b)", nullptr));
}

static void env_finish(struct env *e) {
	db_conn_finish(&e->c);
	hrana_shared_unref(e->shared);
	turso_mock_stop(e->m);
}

static int64_t count(struct db_conn *c) {
	int64_t n = -1;
	assert_true(db_query_i64(c, "SELECT count(*) FROM t", &n, nullptr));
	return n;
}

static void test_values(void **state) {
	(void)state;
	struct env e;
	env_init(&e);
	struct db_stmt *st = db_prepare(&e.c, "INSERT INTO t(a, b) VALUES (:a, ?2)", nullptr);
	bind_text(st, ":a", "héllo \"x\"");
	bind_pos_blob(st, 2, "\x00\x01\xff", 3);
	assert_true(db_step_done(st, nullptr));
	assert_int_equal(db_last_insert_rowid(&e.c), 1);
	assert_int_equal(db_changes(&e.c), 1);
	bind_null(st, ":a");
	bind_pos_i64(st, 2, -42);
	assert_true(db_step_done(st, nullptr));
	db_finalize(st);
	st = db_prepare(&e.c, "SELECT id, a, b, 1.5 AS f FROM t ORDER BY id", nullptr);
	assert_int_equal(db_step(st), SQLITE_ROW);
	char *a = nullptr;
	assert_true(col_text(st, 1, &a, nullptr));
	assert_string_equal(a, "héllo \"x\"");
	xfree(a);
	assert_int_equal(col_type(st, 2), SQLITE_BLOB);
	struct str b = col_bytes(st, 2);
	assert_int_equal(b.len, 3);
	assert_memory_equal(b.p, "\x00\x01\xff", 3);
	assert_int_equal(col_type(st, 3), SQLITE_FLOAT);
	assert_int_equal(db_step(st), SQLITE_ROW);
	assert_int_equal(col_type(st, 1), SQLITE_NULL);
	assert_int_equal(col_i64(st, 2), -42);
	struct err *err = nullptr;
	assert_false(col_text(st, 2, &a, &err));
	assert_string_equal(err_msg(err), "Invalid column type Integer at index: 2, name: b");
	err_free(err);
	assert_int_equal(db_step(st), SQLITE_DONE);
	db_finalize(st);
	// SQL errors, a unique constraint keeping its code
	err = nullptr;
	assert_false(db_exec_one(&e.c, "SELECT * FROM missing", &err));
	assert_non_null(strstr(err_msg(err), "Turso SQL error (SQLITE_ERROR): no such table"));
	err_free(err);
	assert_true(db_exec(&e.c, "CREATE UNIQUE INDEX t_b ON t(b)", nullptr));
	err = nullptr;
	assert_false(db_exec_one(&e.c, "INSERT INTO t(b) VALUES (-42)", &err));
	assert_int_equal(db_errcode(&e.c), SQLITE_CONSTRAINT_UNIQUE);
	err_free(err);
	// The hosted SQL parser's restrictions
	err = nullptr;
	assert_false(db_exec(&e.c, "VACUUM", &err));
	err_free(err);
	env_finish(&e);
}

static void test_transactions(void **state) {
	(void)state;
	struct env e;
	env_init(&e);
	assert_true(db_begin(&e.c, nullptr));
	assert_true(db_exec_one(&e.c, "INSERT INTO t(a) VALUES (1)", nullptr));
	assert_true(db_commit(&e.c, nullptr));
	assert_int_equal(count(&e.c), 1);
	assert_true(db_begin(&e.c, nullptr));
	assert_true(db_exec_one(&e.c, "INSERT INTO t(a) VALUES (2)", nullptr));
	db_rollback(&e.c);
	assert_int_equal(count(&e.c), 1);
	// A lost stream poisons the transaction: its SQL never continues on a
	// new auto-commit stream, and nothing is committed
	assert_true(db_begin(&e.c, nullptr));
	pthread_mutex_lock(&e.m->mu);
	e.m->st.fault_sql = xstrdup("VALUES (3)");
	e.m->st.fault = TURSO_FAULT_LOST_STREAM;
	pthread_mutex_unlock(&e.m->mu);
	struct err *err = nullptr;
	assert_false(db_exec_one(&e.c, "INSERT INTO t(a) VALUES (3)", &err));
	err_free(err);
	err = nullptr;
	assert_false(db_exec_one(&e.c, "INSERT INTO t(a) VALUES (4)", &err));
	assert_string_equal(err_msg(err),
			    "Turso transaction failed; discard it before issuing more SQL");
	err_free(err);
	err = nullptr;
	assert_false(db_commit(&e.c, &err));
	err_free(err);
	assert_int_equal(count(&e.c), 0 + 1);
	// A malformed response to COMMIT is an error, never replayed
	assert_true(db_begin(&e.c, nullptr));
	assert_true(db_exec_one(&e.c, "INSERT INTO t(a) VALUES (5)", nullptr));
	pthread_mutex_lock(&e.m->mu);
	e.m->st.fault_sql = xstrdup("COMMIT");
	e.m->st.fault = TURSO_FAULT_MISSING_RESULTS;
	pthread_mutex_unlock(&e.m->mu);
	err = nullptr;
	assert_false(db_commit(&e.c, &err));
	assert_string_equal(err_msg(err), "incomplete Turso pipeline response");
	err_free(err);
	assert_int_equal(turso_mock_count(e.m, "COMMIT"), 2);
	// The connection works again afterwards
	assert_int_equal(count(&e.c), 2);
	env_finish(&e);
}

static void test_replication_index(void **state) {
	(void)state;
	struct env e;
	env_init(&e);
	assert_true(db_exec_one(&e.c, "INSERT INTO t(a) VALUES (1)", nullptr));
	// The next request, and a fork's, ask to observe the write
	assert_int_equal(count(&e.c), 1);
	pthread_mutex_lock(&e.m->mu);
	const char *last = e.m->st.requests.data[e.m->st.requests.len - 1];
	assert_non_null(strstr(last, "\"replication_index\":\""));
	pthread_mutex_unlock(&e.m->mu);
	struct db_conn fork;
	db_conn_init_remote(&fork, hrana_fork(e.c.hrana));
	assert_int_equal(count(&fork), 1);
	db_conn_finish(&fork);
	// The foreign_keys pragma is sent with every stream
	assert_true(db_exec(&e.c, "PRAGMA foreign_keys = 0", nullptr));
	assert_true(db_exec_one(&e.c, "SELECT 1", nullptr));
	assert_int_equal(turso_mock_count(e.m, "PRAGMA foreign_keys = 0"), 1);
	// Requests without a valid token fail
	struct hrana *bad =
		hrana_new(e.c.hrana != nullptr ? "http://127.0.0.1:1/x" : "", "wrong", e.shared);
	struct err *err = nullptr;
	assert_false(hrana_sequence(bad, "SELECT 1", &err));
	assert_non_null(strstr(err_msg(err), "error sending request"));
	err_free(err);
	hrana_free(bad);
	AUTOFREE char *url = xasprintf("%s/db/tenant/v2/pipeline", e.m->url);
	bad = hrana_new(url, "wrong", e.shared);
	err = nullptr;
	assert_false(hrana_sequence(bad, "SELECT 1", &err));
	assert_string_equal(err_msg(err), "Turso request failed (HTTP 401 Unauthorized)");
	err_free(err);
	hrana_free(bad);
	env_finish(&e);
}

static void stmt(struct hrana_stmt *s, const char *sql, int64_t v) {
	*s = (struct hrana_stmt){.sql = xstrdup(sql)};
	vec_push(&s->binds, ((struct db_bind){.index = 1, .v = {.type = DB_INTEGER, .i = v}}));
}

static void test_atomic_batch(void **state) {
	(void)state;
	struct env e;
	env_init(&e);
	struct hrana_stmt s[3];
	stmt(&s[0], "INSERT INTO t(id) VALUES (?)", 10);
	stmt(&s[1], "INSERT INTO t(id) VALUES (?)", 11);
	stmt(&s[2], "INSERT INTO t(id) VALUES (?)", 10);
	assert_true(hrana_atomic_batch(e.c.hrana, s, 2, nullptr));
	assert_int_equal(count(&e.c), 2);
	// A failing step skips the rest, COMMIT included
	struct err *err = nullptr;
	hrana_stmt_free(&s[0]);
	stmt(&s[0], "INSERT INTO t(id) VALUES (?)", 12);
	assert_false(hrana_atomic_batch(e.c.hrana, &s[2], 1, &err) ||
		     hrana_atomic_batch(e.c.hrana, s, 3, &err));
	assert_non_null(strstr(err_msg(err), "Turso message batch failed (SQLITE_ERROR): "));
	err_free(err);
	assert_int_equal(count(&e.c), 2);
	// Requests over the body budget aren't sent
	struct hrana_stmt big;
	stmt(&big, "INSERT INTO t(id, a) VALUES (?, :a)", 20);
	char *text = xmalloc(HRANA_BODY_BUDGET + 1);
	memset(text, 'x', HRANA_BODY_BUDGET);
	text[HRANA_BODY_BUDGET] = '\0';
	struct db_bind b = {.name = xstrdup(":a"), .v = {.type = DB_TEXT}};
	strbuf_append_cstr(&b.v.b, text);
	xfree(text);
	vec_push(&big.binds, b);
	size_t before = turso_mock_count(e.m, "INSERT");
	err = nullptr;
	assert_false(hrana_atomic_batch(e.c.hrana, &big, 1, &err));
	assert_string_equal(err_msg(err), "Turso message batch exceeds HTTP body budget");
	err_free(err);
	assert_int_equal(turso_mock_count(e.m, "INSERT"), before);
	hrana_stmt_free(&big);
	for (size_t i = 0; i < 3; i++) {
		hrana_stmt_free(&s[i]);
	}
	env_finish(&e);
}

// clang-format off
T_MAIN(cmocka_unit_test(test_stream_redirect_cannot_expose_the_group_token_to_an_untrusted_origin),
       cmocka_unit_test(test_usage_counts_execute_and_batch_results_once),
       cmocka_unit_test(test_values), cmocka_unit_test(test_transactions),
       cmocka_unit_test(test_replication_index), cmocka_unit_test(test_atomic_batch))
// clang-format on
