// Remote SQL over Turso's Hrana HTTP protocol (soju-rs
// database/connection.rs Remote): pipelines on streams identified by
// batons, transactions kept on one stream, replication indexes for
// read-your-writes, and atomic batches. Requests block the calling thread
// (the DB thread, or a message writer thread).
#pragma once

#include <stdint.h>

#include "base/err.h"
#include "base/vec.h"

struct hrana;
struct db_bind;
struct db_value;

void hrana_free(struct hrana *h);
// Runs a statement with its parameters; columns and rows (VEC(db_row)) are
// filled for queries
[[nodiscard]] bool hrana_execute(struct hrana *h, const char *sql, const struct db_bind *binds,
				 size_t n, strvec *columns, void *rows, int64_t *last_id,
				 int64_t *changes, struct err **err);
// Several statements without parameters
[[nodiscard]] bool hrana_sequence(struct hrana *h, const char *sql, struct err **err);
[[nodiscard]] bool hrana_begin(struct hrana *h, struct err **err);
// Never replayed: an ambiguous failure leaves the outcome unknown
[[nodiscard]] bool hrana_commit(struct hrana *h, struct err **err);
// Closes the transaction's stream, which rolls it back
void hrana_rollback(struct hrana *h);
