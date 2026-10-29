// A local Turso Platform API and Hrana server for tests (port of the mock
// in soju-rs src/database/turso/tests.rs): databases are SQLite files in a
// temporary directory, streams are open connections addressed by
// single-use batons. Its decoder is independent of the client's, so
// encoding mistakes can't hide.
#pragma once

#include <pthread.h>
#include <sqlite3.h>
#include <stdint.h>

#include "base/map.h"
#include "base/str.h"
#include "base/vec.h"

enum turso_fault {
	TURSO_FAULT_NONE,
	TURSO_FAULT_LOST_STREAM, // the stream is dropped, the baton is null
	TURSO_FAULT_MISSING_RESULTS, // the last result is missing
	TURSO_FAULT_WRONG_RESPONSE_TYPE,
	TURSO_FAULT_HTTP_AFTER, // executed, then HTTP 503
	TURSO_FAULT_EMPTY_BODY, // executed, then an empty body
};

// Knobs and observations; lock m->mu around accesses while it runs
struct turso_mock_state {
	struct strmap databases; // name -> group (owned char *)
	struct strmap streams; // baton -> struct mock_stream *
	uint64_t next_stream;
	bool fail_create, fail_delete, bad_group;
	char *fail_sql; // a database name whose SQL fails with HTTP 503
	char *fault_sql; // the next request containing this SQL gets fault
	enum turso_fault fault;
	char *delay_sql; // requests containing this SQL are answered after
	unsigned delay_ms;
	char *delay_before_sql; // one request containing this SQL waits first
	unsigned delay_before_ms;
	size_t max_body; // 0: none; bigger requests get HTTP 413
	size_t max_batch_lines; // 0: none; bigger message batches time out
	VEC(size_t) request_sizes;
	strvec requests; // pipeline bodies
	uint64_t replication_index;
	bool close_idle_streams; // close autocommit streams (hosted Turso)
};

struct turso_mock {
	pthread_mutex_t mu;
	struct turso_mock_state st;
	char *root; // directory of the database files
	char url[64];
	int listener;
	pthread_t thread;
	bool stop;
	VEC(pthread_t) handlers;
};

[[nodiscard]] struct turso_mock *turso_mock_start(void);
void turso_mock_stop(struct turso_mock *m);
// The number of pipeline requests containing sql so far
[[nodiscard]] size_t turso_mock_count(struct turso_mock *m, const char *sql);
// Runs SQL directly on a database file (tests inspecting the cloud)
[[nodiscard]] sqlite3 *turso_mock_open(struct turso_mock *m, const char *name);
