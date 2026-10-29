// Turso message writes (soju-rs database/message_buffer.rs): stable message
// IDs reserved ahead, atomic HTTP batches on an independent stream, and an
// in-memory SQLite overlay holding the messages not written yet, queried
// like committed history.
#pragma once

#include <stdatomic.h>

#include "db/db.h"

// A message to write, with its reserved ID
struct db_message_insert {
	int64_t id, network;
	char *target, *raw, *time, *sender;
	char *text; // PRIVMSG/NOTICE text without formatting, or nullptr
};

// Converts a message (time tag, or now)
[[nodiscard]] bool db_message_insert_init(struct db_message_insert *m, int64_t id,
					  int64_t network, const char *target,
					  const struct irc_msg *msg, struct err **err);
void db_message_insert_free(struct db_message_insert *m);
// The bytes it accounts for in the buffers
[[nodiscard]] size_t db_message_insert_bytes(const struct db_message_insert *m);

struct db_message_writer;

// A writer for a user's database, on its own stream; nullptr if the user
// is unknown. Used from one thread at a time.
[[nodiscard]] struct db_message_writer *db_message_writer_new(struct db *db, int64_t user_id,
							     struct err **err);
void db_message_writer_free(struct db_message_writer *w);
// Persists a reservation of count IDs before exposing them: a crash may
// leave gaps, but never reuses IDs held by delivery receipts
[[nodiscard]] bool db_message_writer_reserve(struct db_message_writer *w, int64_t count,
					     int64_t *first, int64_t *last, struct err **err);
// Writes messages, splitting batches over body limits and timeouts;
// *rejected counts the messages a constraint rejected (dropped). Other
// errors keep the batch for a retry.
[[nodiscard]] bool db_message_writer_write(struct db_message_writer *w,
					   const struct db_message_insert *msgs, size_t n,
					   size_t *rejected, struct err **err);

// The overlay: a private in-memory database with the same queries
[[nodiscard]] bool db_message_buffer_open(struct db **out, struct err **err);
[[nodiscard]] bool db_message_buffer_insert(struct db *buf, const struct db_message_insert *m,
					    struct err **err);
[[nodiscard]] bool db_message_buffer_remove(struct db *buf, const struct db_message_insert *msgs,
					    size_t n, struct err **err);
