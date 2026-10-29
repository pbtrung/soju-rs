// The DB thread (PLAN.md 2.1 rule 1): one thread owns the SQLite
// connection and runs jobs in order; each job's completion runs on the
// loop thread. Jobs take owned copies of their inputs and never touch
// loop-thread objects; completions find their target by ID (rule 2).
#pragma once

#include <uv.h>

#include "base/err.h"
#include "db/db.h"

struct db_thread;

// Runs on the DB thread
typedef void (*db_job_fn)(struct db *db, void *data);
// Runs on the loop thread, after the job
typedef void (*db_done_fn)(void *data);

// Takes ownership of db (closed by db_thread_stop). delay_max_ms > 0 makes
// the thread sleep a random 0 to delay_max_ms ms before each job, to widen
// race windows in tests (PLAN.md 2.3 L2 "delayed" mode).
[[nodiscard]] bool db_thread_start(uv_loop_t *loop, struct db *db, unsigned delay_max_ms,
				   struct db_thread **out, struct err **err);
// Queues a job; done may be nullptr
void db_thread_submit(struct db_thread *t, db_job_fn run, db_done_fn done, void *data);
// Runs a job and waits for it (blocking the loop thread), for the call
// sites where soju-rs relies on the result synchronously. done isn't used:
// the caller continues with the result.
void db_thread_call(struct db_thread *t, db_job_fn run, void *data);
// Runs every queued job and its completion, stops the thread, closes the
// database and frees t once the loop has run the handle's close callback
void db_thread_stop(struct db_thread *t);
// The database, for what is safe from any thread (db_is_remote, Turso
// message writers)
[[nodiscard]] struct db *db_thread_db(const struct db_thread *t);
// The number of jobs submitted but not completed
[[nodiscard]] size_t db_thread_pending(const struct db_thread *t);
