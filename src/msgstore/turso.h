// Turso message batching (soju-rs msgstore/turso.rs): one buffer filling
// while another is written on an independent stream. Lines get reserved
// IDs at once and are readable from an in-memory overlay until written.
// Batches hold up to the chathistory limit of lines and leave after 1 s;
// failed ones are retried with backoff.
#pragma once

#include "db/writer.h"

struct batch_store;

[[nodiscard]] struct batch_store *batch_store_new(struct db *db, int64_t user_id, size_t limit);
// Drains nothing: call batch_store_flush first
void batch_store_free(struct batch_store *b);
// Opens the writers and reserves the first IDs (blocking)
[[nodiscard]] bool batch_store_prepare(struct batch_store *b, struct err **err);
// Buffers a line; *id is its database message ID
[[nodiscard]] bool batch_store_append(struct batch_store *b, int64_t network, const char *target,
				      const struct irc_msg *msg, int64_t *id, struct err **err);
// Reaps a finished write, starts the next one when due
[[nodiscard]] bool batch_store_poll(struct batch_store *b, struct err **err);
// Writes everything buffered, waiting (shutdown); fails after 3 attempts
[[nodiscard]] bool batch_store_flush(struct batch_store *b, struct err **err);
// The overlay of lines not written yet (a database queried like history)
[[nodiscard]] struct db *batch_store_index(struct batch_store *b);
