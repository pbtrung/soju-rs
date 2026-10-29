// Remote SQL over Turso's Hrana HTTP protocol (soju-rs
// database/connection.rs Remote and turso/http.rs): pipelines on streams
// identified by batons, transactions kept on one stream and never replayed,
// replication indexes for read-your-writes, and atomic batches. Requests
// block the calling thread (the DB thread, or a message writer thread).
#pragma once

#include <stdatomic.h>
#include <stdint.h>

#include "base/err.h"
#include "base/vec.h"

struct db_bind;
struct yyjson_doc;

// An application budget for message batches, below typical proxy body
// limits; counts the encoded JSON request
static constexpr size_t HRANA_BODY_BUDGET = (size_t)512 * 1024;

// Shared by a connection and its forks: the read-your-writes watermark,
// and the rows read and written reported by Turso (metrics)
struct hrana_shared;

[[nodiscard]] struct hrana_shared *hrana_shared_new(void);
void hrana_shared_unref(struct hrana_shared *s);
void hrana_shared_usage(struct hrana_shared *s, uint64_t *read, uint64_t *written);

struct hrana;

// A connection to a database's pipeline endpoint (".../v2/pipeline")
[[nodiscard]] struct hrana *hrana_new(const char *url, const char *token,
				      struct hrana_shared *shared);
// An independent connection (its own stream) sharing the watermark
[[nodiscard]] struct hrana *hrana_fork(const struct hrana *h);
void hrana_free(struct hrana *h);

// Runs a statement with its parameters. columns (strvec) and rows
// (VEC(db_row) of db/conn.h) are filled for queries.
[[nodiscard]] bool hrana_execute(struct hrana *h, const char *sql, const struct db_bind *binds,
				 size_t n, strvec *columns, void *rows, int64_t *last_id,
				 int64_t *changes, struct err **err);
// Several statements without parameters. "PRAGMA foreign_keys = 0|1" only
// sets the pragma sent at the start of every stream.
[[nodiscard]] bool hrana_sequence(struct hrana *h, const char *sql, struct err **err);
[[nodiscard]] bool hrana_begin(struct hrana *h, struct err **err);
// Never replayed: an ambiguous failure leaves the outcome unknown
[[nodiscard]] bool hrana_commit(struct hrana *h, struct err **err);
// Closes the transaction's stream, which rolls it back
void hrana_rollback(struct hrana *h);

// A statement of an atomic batch
struct hrana_stmt {
	char *sql;
	VEC(struct db_bind) binds;
};

void hrana_stmt_free(struct hrana_stmt *s);

// BEGIN IMMEDIATE, the statements and COMMIT in one HTTP request, each
// step conditional on the previous one: an error skips COMMIT. Fails
// without sending if the request exceeds HRANA_BODY_BUDGET.
[[nodiscard]] bool hrana_atomic_batch(struct hrana *h, const struct hrana_stmt *stmts, size_t n,
				      struct err **err);

// soju-rs stream_endpoint: the pipeline URL of a stream base URL, which
// must be an https Turso host without credentials
[[nodiscard]] bool hrana_stream_endpoint(const char *base, char **out, struct err **err);

// Counts the rows read and written of a pipeline response (exposed for
// tests)
void hrana_observe(struct hrana_shared *s, struct yyjson_doc *doc);

// A blocking HTTP request with a bearer token and an optional JSON body
// (soju-rs turso/http.rs): *status and the JSON response (nullptr for an
// empty body or an error status), 30 s timeout, no redirects
[[nodiscard]] bool hrana_http(struct hrana_shared *usage, const char *method, const char *url,
			      const char *token, const char *body, size_t body_len, long *status,
			      struct yyjson_doc **out, struct err **err);
