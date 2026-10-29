// Tests of the DB thread and the CPU-job limiter (no Rust counterpart:
// soju-rs uses tokio's blocking pool). Run under TSan too.
#include "testsupport.h"

#include <stdatomic.h>
#include <stdio.h>

#include "base/alloc.h"
#include "base/cpuwork.h"
#include "db/dbthread.h"

struct store_job {
	char name[32];
	int64_t id; // written by the DB thread, read after completion
	bool ok;
	int *completed;
	int order;
	int *last_order;
};

static void run_store(struct db *db, void *data) {
	struct store_job *j = data;
	struct db_user u;
	db_user_init(&u, j->name);
	j->ok = db_store_user(db, &u, nullptr);
	j->id = u.id;
	db_user_free(&u);
}

static void done_store(void *data) {
	struct store_job *j = data;
	assert_true(j->ok);
	assert_true(j->id > 0);
	// Completions run in submission order
	assert_true(j->order > *j->last_order);
	*j->last_order = j->order;
	(*j->completed)++;
	xfree(j);
}

struct count_job {
	size_t users;
};

static void run_count(struct db *db, void *data) {
	struct count_job *c = data;
	struct db_stats st;
	if (db_stats(db, &st, nullptr)) {
		c->users = (size_t)st.users;
	}
}

static void run_jobs(unsigned delay) {
	uv_loop_t loop;
	assert_int_equal(uv_loop_init(&loop), 0);
	struct db *db = nullptr;
	assert_true(db_open_in_memory(&db, nullptr));
	struct db_thread *t = nullptr;
	assert_true(db_thread_start(&loop, db, delay, &t, nullptr));
	int completed = 0, last_order = -1;
	for (int i = 0; i < 20; i++) {
		struct store_job *j = xcalloc(1, sizeof(*j));
		snprintf(j->name, sizeof(j->name), "user%d", i);
		j->completed = &completed;
		j->order = i;
		j->last_order = &last_order;
		db_thread_submit(t, run_store, done_store, j);
	}
	assert_int_equal(db_thread_pending(t), 20);
	// A synchronous call runs after the queued jobs
	struct count_job c = {0};
	db_thread_call(t, run_count, &c);
	assert_int_equal(c.users, 20);
	while (completed < 10) {
		uv_run(&loop, UV_RUN_ONCE);
	}
	// Stopping runs the remaining completions
	db_thread_stop(t);
	assert_int_equal(completed, 20);
	uv_run(&loop, UV_RUN_DEFAULT);
	assert_int_equal(uv_loop_close(&loop), 0);
}

static void test_jobs_in_order(void **state) {
	(void)state;
	run_jobs(0);
}

static void test_jobs_delayed(void **state) {
	(void)state;
	run_jobs(2);
}

static atomic_int busy_runs;

static void busy_work(void *data) {
	(void)data;
	atomic_fetch_add(&busy_runs, 1);
}

static void busy_done(void *data) {
	int *done = data;
	(*done)++;
}

static void test_cpu_work_limit(void **state) {
	(void)state;
	uv_loop_t loop;
	assert_int_equal(uv_loop_init(&loop), 0);
	struct cpu_work_pool *p = cpu_work_pool_new(&loop, 1);
	int done = 0;
	// One runs, 64 wait, the next one fails
	for (size_t i = 0; i < 1 + CPU_WORK_MAX_WAITING; i++) {
		assert_true(cpu_work_submit(p, busy_work, busy_done, &done, nullptr));
	}
	assert_int_equal(cpu_work_running(p), 1);
	assert_int_equal(cpu_work_waiting(p), CPU_WORK_MAX_WAITING);
	struct err *e = nullptr;
	assert_false(cpu_work_submit(p, busy_work, busy_done, &done, &e));
	assert_string_equal(err_msg(e), "server busy, try again later");
	err_free(e);
	uv_run(&loop, UV_RUN_DEFAULT);
	assert_int_equal(done, 1 + CPU_WORK_MAX_WAITING);
	assert_int_equal(atomic_load(&busy_runs), 1 + (int)CPU_WORK_MAX_WAITING);
	assert_int_equal(cpu_work_running(p), 0);
	cpu_work_pool_free(p);
	assert_int_equal(uv_loop_close(&loop), 0);
}

T_MAIN(cmocka_unit_test(test_jobs_in_order), cmocka_unit_test(test_jobs_delayed),
       cmocka_unit_test(test_cpu_work_limit))
