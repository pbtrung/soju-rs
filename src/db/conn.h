// Connections and statements of the src/db/*.c queries (soju-rs
// database/connection.rs): a local SQLite connection, or a remote Turso one
// speaking Hrana over HTTP. The SQL, parameter binding and row decoding stay
// the same for both. Steps return SQLite result codes; on errors,
// db_sql_err reports the connection's last error.
#pragma once

#include <sqlite3.h>

#include "base/vec.h"
#include "db/db.h"

struct hrana;

enum db_value_type {
	DB_NULL = SQLITE_NULL,
	DB_INTEGER = SQLITE_INTEGER,
	DB_FLOAT = SQLITE_FLOAT,
	DB_TEXT = SQLITE_TEXT,
	DB_BLOB = SQLITE_BLOB,
};

// A remote parameter or column value
struct db_value {
	enum db_value_type type;
	int64_t i;
	double f;
	struct strbuf b; // text or blob bytes
};

void db_value_free(struct db_value *v);

struct db_conn {
	bool remote;
	sqlite3 *sqlite; // local
	struct hrana *hrana; // remote
	// store_messages runs for every logged message: kept compiled (local)
	struct db_stmt *insert_target;
	struct db_stmt *insert_message;
	// the last remote error (local connections ask SQLite)
	int err_code;
	char *err_msg;
	int64_t last_id, changes; // of the last remote statement
};

struct db_bind {
	char *name; // ":name", or nullptr for a positional parameter
	int index; // positional (1-based)
	struct db_value v;
};

typedef VEC(struct db_value) db_row;
typedef VEC(db_row) db_rows;

struct db_stmt {
	struct db_conn *conn;
	sqlite3_stmt *local;
	// remote: executed by the first step
	char *sql;
	VEC(struct db_bind) binds;
	strvec columns;
	db_rows rows;
	size_t next_row;
	bool executed;
};

// A local connection over an open handle (taken)
void db_conn_init_local(struct db_conn *c, sqlite3 *handle);
// A remote connection (takes h)
void db_conn_init_remote(struct db_conn *c, struct hrana *h);
// Closes the connection; aborts if a local statement is left (PLAN.md 2.4)
void db_conn_finish(struct db_conn *c);

// Sets *err from the connection's last error; returns false
bool db_sql_err(struct db_conn *c, struct err **err);
// The extended result code of the last error
[[nodiscard]] int db_errcode(struct db_conn *c);
// nullptr on error
[[nodiscard]] struct db_stmt *db_prepare(struct db_conn *c, const char *sql, struct err **err);
// SQLITE_ROW, SQLITE_DONE or an error code
[[nodiscard]] int db_step(struct db_stmt *st);
void db_reset(struct db_stmt *st);
void db_clear_bindings(struct db_stmt *st);
void db_finalize(struct db_stmt *st);
[[nodiscard]] int64_t db_last_insert_rowid(struct db_conn *c);
[[nodiscard]] int64_t db_changes(struct db_conn *c);

// Several statements, without parameters nor results
[[nodiscard]] bool db_exec(struct db_conn *c, const char *sql, struct err **err);
// Steps a statement which returns no rows, then resets it
[[nodiscard]] bool db_step_done(struct db_stmt *st, struct err **err);
// Prepares, binds nothing, steps once
[[nodiscard]] bool db_exec_one(struct db_conn *c, const char *sql, struct err **err);
// Like db_exec_one with one integer bound to ?1
[[nodiscard]] bool db_exec_i64(struct db_conn *c, const char *sql, int64_t v, struct err **err);
// The integer in the first column of the first row
[[nodiscard]] bool db_query_i64(struct db_conn *c, const char *sql, int64_t *out, struct err **err);
[[nodiscard]] bool db_begin(struct db_conn *c, struct err **err);
[[nodiscard]] bool db_commit(struct db_conn *c, struct err **err);
void db_rollback(struct db_conn *c);

// Binding by name (":name"); a missing name is a programming error
void bind_i64(struct db_stmt *st, const char *name, int64_t v);
void bind_text(struct db_stmt *st, const char *name, const char *s);
// NULL for "" (null_string in soju-rs)
void bind_text_or_null(struct db_stmt *st, const char *name, const char *s);
void bind_null(struct db_stmt *st, const char *name);
void bind_blob(struct db_stmt *st, const char *name, const struct db_blob *b);
// A blob of len bytes
void bind_bytes(struct db_stmt *st, const char *name, const void *p, size_t len);
// The server-time text, or NULL
void bind_time(struct db_stmt *st, const char *name, const struct db_time *t);
// Positional binding (?1, ?2...)
void bind_pos_i64(struct db_stmt *st, int index, int64_t v);
void bind_pos_text(struct db_stmt *st, int index, const char *s);
void bind_pos_null(struct db_stmt *st, int index);
// A blob of len bytes
void bind_pos_blob(struct db_stmt *st, int index, const void *p, size_t len);

[[nodiscard]] int col_type(struct db_stmt *st, int i);
// A text column decoded lossily, NULL read as "" (lossy_string)
[[nodiscard]] char *col_lossy(struct db_stmt *st, int i);
// A text column which must not be NULL nor invalid UTF-8 (rusqlite's
// String conversion)
[[nodiscard]] bool col_text(struct db_stmt *st, int i, char **out, struct err **err);
// Optional text: NULL gives "" (unwrap_or_default)
[[nodiscard]] bool col_text_opt(struct db_stmt *st, int i, char **out, struct err **err);
[[nodiscard]] int64_t col_i64(struct db_stmt *st, int i);
[[nodiscard]] bool col_bool(struct db_stmt *st, int i);
// An optional time in the server-time layout (time_from_sql)
[[nodiscard]] bool col_time(struct db_stmt *st, int i, struct db_time *out, struct err **err);
// An optional blob
void col_blob(struct db_stmt *st, int i, struct db_blob *out);
// The raw bytes of a text or blob column (valid until the next step)
[[nodiscard]] struct str col_bytes(struct db_stmt *st, int i);
