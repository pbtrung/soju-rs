// Helpers shared by the src/db/*.c files: statements, binding by name,
// reading columns with soju-rs's conversions.
#pragma once

#include <sqlite3.h>

#include "db/db.h"

struct db {
	sqlite3 *conn;
	// store_messages runs for every logged message: kept compiled
	sqlite3_stmt *insert_target;
	sqlite3_stmt *insert_message;
};

// Sets *err from the connection's last error
bool db_sql_err(struct db *db, struct err **err);
// nullptr on error
[[nodiscard]] sqlite3_stmt *db_prepare(struct db *db, const char *sql, struct err **err);
[[nodiscard]] bool db_exec(struct db *db, const char *sql, struct err **err);
// Steps a statement which returns no rows; *changes may be nullptr
[[nodiscard]] bool db_step_done(struct db *db, sqlite3_stmt *st, struct err **err);
// Prepares, binds nothing, steps once
[[nodiscard]] bool db_exec_one(struct db *db, const char *sql, struct err **err);
// Like db_exec_one with one integer bound to ?1
[[nodiscard]] bool db_exec_i64(struct db *db, const char *sql, int64_t v, struct err **err);
[[nodiscard]] bool db_begin(struct db *db, struct err **err);
[[nodiscard]] bool db_commit(struct db *db, struct err **err);
void db_rollback(struct db *db);

// Binding by name (":name"); a missing name is a programming error
void bind_i64(sqlite3_stmt *st, const char *name, int64_t v);
void bind_text(sqlite3_stmt *st, const char *name, const char *s);
// NULL for "" (null_string in soju-rs)
void bind_text_or_null(sqlite3_stmt *st, const char *name, const char *s);
void bind_null(sqlite3_stmt *st, const char *name);
void bind_blob(sqlite3_stmt *st, const char *name, const struct db_blob *b);
// The server-time text, or NULL
void bind_time(sqlite3_stmt *st, const char *name, const struct db_time *t);

// A text column decoded lossily, NULL read as "" (lossy_string)
[[nodiscard]] char *col_lossy(sqlite3_stmt *st, int i);
// A text column which must not be NULL nor invalid UTF-8 (rusqlite's
// String conversion)
[[nodiscard]] bool col_text(sqlite3_stmt *st, int i, char **out, struct err **err);
// Optional text: NULL gives "" (unwrap_or_default)
[[nodiscard]] bool col_text_opt(sqlite3_stmt *st, int i, char **out, struct err **err);
[[nodiscard]] int64_t col_i64(sqlite3_stmt *st, int i);
[[nodiscard]] bool col_bool(sqlite3_stmt *st, int i);
// An optional time in the server-time layout (time_from_sql)
[[nodiscard]] bool col_time(sqlite3_stmt *st, int i, struct db_time *out, struct err **err);
// An optional blob
void col_blob(sqlite3_stmt *st, int i, struct db_blob *out);

[[nodiscard]] bool record_message_id_high_water(struct db *db, struct err **err);
