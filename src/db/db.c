#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "base/alloc.h"
#include "base/utf8.h"
#include "db/internal.h"

static const char SCHEMA[] = {
#embed "sqlite_schema.sql"
	, '\0'};
static const char FTS_SCHEMA[] = {
#embed "message_fts.sql"
	, '\0'};

// SQLite schema migrations, including upstream soju's. Migration #0 is
// reserved for schema initialization.
static const char *const MIGRATIONS[] = {
#include "db/migrations.inc"
};

static constexpr int64_t SCHEMA_VERSION = sizeof(MIGRATIONS) / sizeof(MIGRATIONS[0]);
static_assert(SCHEMA_VERSION == 33);

int64_t db_schema_version(void) {
	return SCHEMA_VERSION;
}

const char *db_migration(size_t i) {
	return i < (size_t)SCHEMA_VERSION ? MIGRATIONS[i] : nullptr;
}

bool db_sql_err(struct db *db, struct err **err) {
	return err_set(err, sqlite3_extended_errcode(db->conn), "%s", sqlite3_errmsg(db->conn));
}

sqlite3_stmt *db_prepare(struct db *db, const char *sql, struct err **err) {
	sqlite3_stmt *st = nullptr;
	if (sqlite3_prepare_v2(db->conn, sql, -1, &st, nullptr) != SQLITE_OK) {
		db_sql_err(db, err);
		sqlite3_finalize(st);
		return nullptr;
	}
	return st;
}

bool db_exec(struct db *db, const char *sql, struct err **err) {
	char *msg = nullptr;
	int rc = sqlite3_exec(db->conn, sql, nullptr, nullptr, &msg);
	if (rc != SQLITE_OK) {
		err_set(err, sqlite3_extended_errcode(db->conn), "%s",
			msg != nullptr ? msg : sqlite3_errstr(rc));
		sqlite3_free(msg);
		return false;
	}
	return true;
}

bool db_step_done(struct db *db, sqlite3_stmt *st, struct err **err) {
	int rc;
	while ((rc = sqlite3_step(st)) == SQLITE_ROW) {
	}
	if (rc != SQLITE_DONE) {
		db_sql_err(db, err);
		sqlite3_reset(st);
		return false;
	}
	sqlite3_reset(st);
	return true;
}

bool db_exec_one(struct db *db, const char *sql, struct err **err) {
	sqlite3_stmt *st = db_prepare(db, sql, err);
	if (st == nullptr) {
		return false;
	}
	bool ok = db_step_done(db, st, err);
	sqlite3_finalize(st);
	return ok;
}

bool db_exec_i64(struct db *db, const char *sql, int64_t v, struct err **err) {
	sqlite3_stmt *st = db_prepare(db, sql, err);
	if (st == nullptr) {
		return false;
	}
	sqlite3_bind_int64(st, 1, v);
	bool ok = db_step_done(db, st, err);
	sqlite3_finalize(st);
	return ok;
}

bool db_begin(struct db *db, struct err **err) {
	return db_exec(db, "BEGIN IMMEDIATE", err);
}

bool db_commit(struct db *db, struct err **err) {
	return db_exec(db, "COMMIT", err);
}

void db_rollback(struct db *db) {
	if (!sqlite3_get_autocommit(db->conn)) {
		(void)db_exec(db, "ROLLBACK", nullptr);
	}
}

static int param(sqlite3_stmt *st, const char *name) {
	int i = sqlite3_bind_parameter_index(st, name);
	if (i == 0) {
		fprintf(stderr, "soju: SQL parameter %s missing in %s\n", name, sqlite3_sql(st));
		abort();
	}
	return i;
}

void bind_i64(sqlite3_stmt *st, const char *name, int64_t v) {
	sqlite3_bind_int64(st, param(st, name), v);
}

void bind_text(sqlite3_stmt *st, const char *name, const char *s) {
	sqlite3_bind_text(st, param(st, name), s, -1, SQLITE_TRANSIENT);
}

void bind_text_or_null(sqlite3_stmt *st, const char *name, const char *s) {
	if (s[0] == '\0') {
		sqlite3_bind_null(st, param(st, name));
	} else {
		bind_text(st, name, s);
	}
}

void bind_null(sqlite3_stmt *st, const char *name) {
	sqlite3_bind_null(st, param(st, name));
}

void bind_blob(sqlite3_stmt *st, const char *name, const struct db_blob *b) {
	if (!b->set) {
		bind_null(st, name);
		return;
	}
	sqlite3_bind_blob64(st, param(st, name), b->data.len > 0 ? b->data.buf : "", b->data.len,
			    SQLITE_TRANSIENT);
}

void bind_time(sqlite3_stmt *st, const char *name, const struct db_time *t) {
	if (!t->set) {
		bind_null(st, name);
		return;
	}
	AUTOFREE char *s = format_server_time_cstr(t->t);
	bind_text(st, name, s);
}

static struct str col_str(sqlite3_stmt *st, int i) {
	const unsigned char *p = sqlite3_column_text(st, i);
	return str_new(p != nullptr ? (const char *)p : "", (size_t)sqlite3_column_bytes(st, i));
}

char *col_lossy(sqlite3_stmt *st, int i) {
	if (sqlite3_column_type(st, i) == SQLITE_NULL) {
		return xstrdup("");
	}
	struct strbuf sb = STRBUF_INIT;
	utf8_lossy(&sb, col_str(st, i));
	return strbuf_take(&sb);
}

static const char *type_name(int type) {
	switch (type) {
	case SQLITE_INTEGER:
		return "Integer";
	case SQLITE_FLOAT:
		return "Real";
	case SQLITE_BLOB:
		return "Blob";
	case SQLITE_NULL:
		return "Null";
	default:
		return "Text";
	}
}

bool col_text(sqlite3_stmt *st, int i, char **out, struct err **err) {
	int type = sqlite3_column_type(st, i);
	if (type != SQLITE_TEXT) {
		// rusqlite: InvalidColumnType
		const char *name = sqlite3_column_name(st, i);
		err_set(err, 0, "Invalid column type %s at index: %d, name: %s", type_name(type), i,
			name != nullptr ? name : "");
		return false;
	}
	struct str s = col_str(st, i);
	if (!utf8_valid(s)) {
		err_set(err, 0, "Conversion error from type Text at index: %d, invalid utf-8", i);
		return false;
	}
	*out = str_dup(s);
	return true;
}

bool col_text_opt(sqlite3_stmt *st, int i, char **out, struct err **err) {
	if (sqlite3_column_type(st, i) == SQLITE_NULL) {
		*out = xstrdup("");
		return true;
	}
	return col_text(st, i, out, err);
}

int64_t col_i64(sqlite3_stmt *st, int i) {
	return sqlite3_column_int64(st, i);
}

bool col_bool(sqlite3_stmt *st, int i) {
	return sqlite3_column_int64(st, i) != 0;
}

bool col_time(sqlite3_stmt *st, int i, struct db_time *out, struct err **err) {
	*out = (struct db_time){0};
	if (sqlite3_column_type(st, i) == SQLITE_NULL) {
		return true;
	}
	AUTOFREE char *s = nullptr;
	if (!col_text(st, i, &s, err)) {
		return false;
	}
	if (!parse_server_time(str_from(s), &out->t)) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(s));
		return err_set(err, 0, "cannot parse time %s", q);
	}
	out->set = true;
	return true;
}

void col_blob(sqlite3_stmt *st, int i, struct db_blob *out) {
	*out = (struct db_blob){0};
	if (sqlite3_column_type(st, i) == SQLITE_NULL) {
		return;
	}
	out->set = true;
	const void *p = sqlite3_column_blob(st, i);
	strbuf_append(&out->data, p != nullptr ? p : "", (size_t)sqlite3_column_bytes(st, i));
	strbuf_append(&out->data, "", 0);
}

static bool query_i64(struct db *db, const char *sql, int64_t *out, struct err **err) {
	sqlite3_stmt *st = db_prepare(db, sql, err);
	if (st == nullptr) {
		return false;
	}
	int rc = sqlite3_step(st);
	bool ok = rc == SQLITE_ROW;
	if (ok) {
		*out = sqlite3_column_int64(st, 0);
	} else if (rc == SQLITE_DONE) {
		err_set(err, 0, "Query returned no rows");
	} else {
		db_sql_err(db, err);
	}
	sqlite3_finalize(st);
	return ok;
}

static bool user_version(struct db *db, int64_t *out, struct err **err) {
	return query_i64(db, "PRAGMA user_version", out, err);
}

static bool set_user_version(struct db *db, int64_t v, struct err **err) {
	char sql[64];
	snprintf(sql, sizeof(sql), "PRAGMA user_version = %lld", (long long)v);
	return db_exec(db, sql, err);
}

// "<prefix>: <message of e>"
static bool wrap(struct err **err, struct err *e, const char *prefix) {
	err_set(err, e != nullptr ? e->code : 0, "%s: %s", prefix, err_msg(e));
	err_free(e);
	return false;
}

static bool run_migrations(struct db *db, int64_t version, struct err **err) {
	struct err *e = nullptr;
	if (!db_begin(db, err)) {
		return false;
	}
	if (version == 0) {
		if (!db_exec(db, SCHEMA, &e)) {
			wrap(err, e, "failed to initialize schema");
			goto fail;
		}
		if (!db_exec(db, FTS_SCHEMA, err)) {
			goto fail;
		}
	} else {
		for (int64_t i = version; i < SCHEMA_VERSION; i++) {
			if (!db_exec(db, MIGRATIONS[i], &e)) {
				char prefix[64];
				snprintf(prefix, sizeof(prefix),
					 "failed to execute migration #%lld", (long long)i);
				wrap(err, e, prefix);
				goto fail;
			}
		}
		if (version < 32 &&
		    (!db_exec(db,
			      "DROP TRIGGER MessageFTSInsert; DROP TRIGGER MessageFTSDelete; DROP "
			      "TRIGGER MessageFTSUpdate; DROP TABLE MessageFTS;",
			      err) ||
		     !db_exec(db, FTS_SCHEMA, err) ||
		     !db_exec(db,
			      "INSERT INTO MessageFTS(rowid, text) SELECT id, text FROM Message "
			      "WHERE text IS NOT NULL;",
			      err))) {
			goto fail;
		}
	}
	if (!set_user_version(db, SCHEMA_VERSION, &e)) {
		wrap(err, e, "failed to bump schema version");
		goto fail;
	}
	if (!db_commit(db, err)) {
		goto fail;
	}
	return true;
fail:
	db_rollback(db);
	return false;
}

bool db_upgrade(struct db *db, struct err **err) {
	int64_t version;
	if (!user_version(db, &version, err)) {
		return false;
	}
	if (version == SCHEMA_VERSION) {
		return true;
	}
	if (version > SCHEMA_VERSION) {
		return err_set(err, 0, "soju (version %lld) older than schema (version %lld)",
			       (long long)SCHEMA_VERSION, (long long)version);
	}
	// Some migrations rebuild tables referenced by foreign keys. As
	// recommended by SQLite, disable foreign key enforcement while altering
	// the schema (this can't be changed inside a transaction).
	if (!db_exec(db, "PRAGMA foreign_keys = 0", err)) {
		return false;
	}
	bool ok = run_migrations(db, version, err);
	if (!db_exec(db, "PRAGMA foreign_keys = 1", ok ? err : nullptr)) {
		return false;
	}
	return ok;
}

static bool setup(struct db *db, struct err **err) {
	if (sqlite3_busy_timeout(db->conn, 5000) != SQLITE_OK ||
	    !db_exec(db, "PRAGMA foreign_keys = 1", err)) {
		return db_sql_err(db, err);
	}
	// journal_mode returns a row
	sqlite3_stmt *st = db_prepare(db, "PRAGMA journal_mode = WAL", err);
	if (st == nullptr) {
		return false;
	}
	int rc = sqlite3_step(st);
	sqlite3_finalize(st);
	if (rc != SQLITE_ROW) {
		return db_sql_err(db, err);
	}
	if (!db_exec(db, "PRAGMA synchronous = NORMAL", err) ||
	    !db_exec(db, "PRAGMA cache_size = -512", err) || !db_upgrade(db, err)) {
		return false;
	}
	// Not part of upstream's schema, see record_message_id_high_water
	return db_exec(db,
		       "CREATE TABLE IF NOT EXISTS MessageIdHighWater (\n"
		       "                id INTEGER PRIMARY KEY CHECK (id = 0),\n"
		       "                last_id INTEGER NOT NULL\n"
		       "            );",
		       err);
}

bool db_open_handle(sqlite3 *conn, struct db **out, struct err **err) {
	struct db *db = xcalloc(1, sizeof(*db));
	db->conn = conn;
	if (!setup(db, err)) {
		db_close(db);
		return false;
	}
	*out = db;
	return true;
}

static bool open_path(const char *path, struct db **out, struct err **err) {
	sqlite3 *conn = nullptr;
	int rc = sqlite3_open_v2(
		path, &conn, SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_URI, nullptr);
	if (rc != SQLITE_OK) {
		err_set(err, rc, "%s", conn != nullptr ? sqlite3_errmsg(conn) : sqlite3_errstr(rc));
		sqlite3_close(conn);
		return false;
	}
	return db_open_handle(conn, out, err);
}

bool db_open(const char *source, struct db **out, struct err **err) {
	return open_path(source, out, err);
}

bool db_open_in_memory(struct db **out, struct err **err) {
	return open_path(":memory:", out, err);
}

void db_close(struct db *db) {
	if (db == nullptr) {
		return;
	}
	sqlite3_finalize(db->insert_target);
	sqlite3_finalize(db->insert_message);
	int rc = sqlite3_close(db->conn);
	if (rc != SQLITE_OK) {
		// An unfinalized statement: a bug (PLAN.md 2.4)
		fprintf(stderr, "soju: sqlite3_close: %s\n", sqlite3_errstr(rc));
		abort();
	}
	xfree(db);
}

sqlite3 *db_handle(struct db *db) {
	return db->conn;
}

bool db_stats(struct db *db, struct db_stats *out, struct err **err) {
	sqlite3_stmt *st =
		db_prepare(db,
			   "SELECT\n                (SELECT COUNT(*) FROM User) AS users,\n"
			   "                (SELECT COUNT(*) FROM Network) AS networks,\n"
			   "                (SELECT COUNT(*) FROM Channel) AS channels",
			   err);
	if (st == nullptr) {
		return false;
	}
	bool ok = sqlite3_step(st) == SQLITE_ROW;
	if (ok) {
		*out = (struct db_stats){col_i64(st, 0), col_i64(st, 1), col_i64(st, 2)};
	} else {
		db_sql_err(db, err);
	}
	sqlite3_finalize(st);
	return ok;
}

bool db_free_page_ratio(struct db *db, double *out, struct err **err) {
	int64_t pages, free_pages;
	if (!query_i64(db, "PRAGMA page_count", &pages, err) ||
	    !query_i64(db, "PRAGMA freelist_count", &free_pages, err)) {
		return false;
	}
	*out = pages > 0 ? (double)free_pages / (double)pages : 0.0;
	return true;
}

static bool checkpoint(struct db *db, struct err **err) {
	int64_t ignored;
	return query_i64(db, "PRAGMA wal_checkpoint(TRUNCATE)", &ignored, err);
}

bool db_optimize(struct db *db, struct err **err) {
	return db_exec(db, "INSERT INTO MessageFTS(MessageFTS) VALUES ('optimize');", err) &&
	       checkpoint(db, err);
}

bool db_shrink_memory(struct db *db, struct err **err) {
	return db_exec(db, "PRAGMA shrink_memory;", err);
}

bool db_vacuum(struct db *db, struct err **err) {
	// VACUUM goes through the WAL, which keeps its size on disk until
	// truncated by a checkpoint
	return db_exec(db, "VACUUM;", err) && checkpoint(db, err);
}
