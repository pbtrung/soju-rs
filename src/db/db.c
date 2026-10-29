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

static bool user_version(struct db_conn *c, int64_t *out, struct err **err) {
	return db_query_i64(c, "PRAGMA user_version", out, err);
}

static bool set_user_version(struct db_conn *c, int64_t v, struct err **err) {
	char sql[64];
	snprintf(sql, sizeof(sql), "PRAGMA user_version = %lld", (long long)v);
	return db_exec(c, sql, err);
}

// "<prefix>: <message of e>"
static bool wrap(struct err **err, struct err *e, const char *prefix) {
	err_set(err, e != nullptr ? e->code : 0, "%s: %s", prefix, err_msg(e));
	err_free(e);
	return false;
}

static bool run_migrations(struct db_conn *c, int64_t version, struct err **err) {
	struct err *e = nullptr;
	if (!db_begin(c, err)) {
		return false;
	}
	if (version == 0) {
		if (!db_exec(c, SCHEMA, &e)) {
			wrap(err, e, "failed to initialize schema");
			goto fail;
		}
		if (!db_exec(c, FTS_SCHEMA, err)) {
			goto fail;
		}
	} else {
		for (int64_t i = version; i < SCHEMA_VERSION; i++) {
			if (!db_exec(c, MIGRATIONS[i], &e)) {
				char prefix[64];
				snprintf(prefix, sizeof(prefix),
					 "failed to execute migration #%lld", (long long)i);
				wrap(err, e, prefix);
				goto fail;
			}
		}
		if (version < 32 &&
		    (!db_exec(c,
			      "DROP TRIGGER MessageFTSInsert; DROP TRIGGER MessageFTSDelete; DROP "
			      "TRIGGER MessageFTSUpdate; DROP TABLE MessageFTS;",
			      err) ||
		     !db_exec(c, FTS_SCHEMA, err) ||
		     !db_exec(c,
			      "INSERT INTO MessageFTS(rowid, text) SELECT id, text FROM Message "
			      "WHERE text IS NOT NULL;",
			      err))) {
			goto fail;
		}
	}
	if (!set_user_version(c, SCHEMA_VERSION, &e)) {
		wrap(err, e, "failed to bump schema version");
		goto fail;
	}
	if (!db_commit(c, err)) {
		goto fail;
	}
	return true;
fail:
	db_rollback(c);
	return false;
}

bool db_upgrade(struct db *db, struct err **err) {
	struct db_conn *c = &db->account;
	int64_t version;
	if (!user_version(c, &version, err)) {
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
	if (!db_exec(c, "PRAGMA foreign_keys = 0", err)) {
		return false;
	}
	bool ok = run_migrations(c, version, err);
	if (!db_exec(c, "PRAGMA foreign_keys = 1", ok ? err : nullptr)) {
		return false;
	}
	return ok;
}

static bool setup(struct db *db, struct err **err) {
	struct db_conn *c = &db->account;
	if (sqlite3_busy_timeout(c->sqlite, 5000) != SQLITE_OK ||
	    !db_exec(c, "PRAGMA foreign_keys = 1", err)) {
		return db_sql_err(c, err);
	}
	// journal_mode returns a row
	struct db_stmt *st = db_prepare(c, "PRAGMA journal_mode = WAL", err);
	if (st == nullptr) {
		return false;
	}
	int rc = db_step(st);
	db_finalize(st);
	if (rc != SQLITE_ROW) {
		return db_sql_err(c, err);
	}
	if (!db_exec(c, "PRAGMA synchronous = NORMAL", err) ||
	    !db_exec(c, "PRAGMA cache_size = -512", err) || !db_upgrade(db, err)) {
		return false;
	}
	// Local high-water marks keep asynchronous actor and message IDs from
	// being reused after deletion. They do not change upstream user_version.
	return db_exec(
		c,
		"CREATE TABLE IF NOT EXISTS MessageIdHighWater (\n"
		"                id INTEGER PRIMARY KEY CHECK (id = 0),\n"
		"                last_id INTEGER NOT NULL\n"
		"            );"
		"CREATE TABLE IF NOT EXISTS ActorIdHighWater (kind TEXT PRIMARY KEY, last_id "
		"INTEGER NOT NULL);"
		"CREATE TRIGGER IF NOT EXISTS actor_user_deleted BEFORE DELETE ON User BEGIN "
		"INSERT INTO ActorIdHighWater VALUES ('user', OLD.id) ON CONFLICT(kind) DO UPDATE "
		"SET last_id=max(last_id,excluded.last_id); END;"
		"CREATE TRIGGER IF NOT EXISTS actor_network_deleted BEFORE DELETE ON Network BEGIN "
		"INSERT INTO ActorIdHighWater VALUES ('network', OLD.id) ON CONFLICT(kind) DO "
		"UPDATE SET last_id=max(last_id,excluded.last_id); END;",
		err);
}

bool db_open_handle(sqlite3 *conn, struct db **out, struct err **err) {
	struct db *db = xcalloc(1, sizeof(*db));
	db_conn_init_local(&db->account, conn);
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
	db_conn_finish(&db->account);
	xfree(db);
}

sqlite3 *db_handle(struct db *db) {
	return db->account.sqlite;
}

bool db_stats(struct db *db, struct db_stats *out, struct err **err) {
	struct db_conn *c = &db->account;
	struct db_stmt *st =
		db_prepare(c,
			   "SELECT\n                (SELECT COUNT(*) FROM User) AS users,\n"
			   "                (SELECT COUNT(*) FROM Network) AS networks,\n"
			   "                (SELECT COUNT(*) FROM Channel) AS channels",
			   err);
	if (st == nullptr) {
		return false;
	}
	bool ok = db_step(st) == SQLITE_ROW;
	if (ok) {
		*out = (struct db_stats){col_i64(st, 0), col_i64(st, 1), col_i64(st, 2)};
	} else {
		db_sql_err(c, err);
	}
	db_finalize(st);
	return ok;
}

bool db_free_page_ratio(struct db *db, double *out, struct err **err) {
	int64_t pages, free_pages;
	if (!db_query_i64(&db->account, "PRAGMA page_count", &pages, err) ||
	    !db_query_i64(&db->account, "PRAGMA freelist_count", &free_pages, err)) {
		return false;
	}
	*out = pages > 0 ? (double)free_pages / (double)pages : 0.0;
	return true;
}

static bool checkpoint(struct db *db, struct err **err) {
	int64_t ignored;
	return db_query_i64(&db->account, "PRAGMA wal_checkpoint(TRUNCATE)", &ignored, err);
}

bool db_optimize(struct db *db, struct err **err) {
	return db_exec(&db->account, "INSERT INTO MessageFTS(MessageFTS) VALUES ('optimize');",
		       err) &&
	       checkpoint(db, err);
}

bool db_shrink_memory(struct db *db, struct err **err) {
	return db_exec(&db->account, "PRAGMA shrink_memory;", err);
}

bool db_vacuum(struct db *db, struct err **err) {
	// VACUUM goes through the WAL, which keeps its size on disk until
	// truncated by a checkpoint
	return db_exec(&db->account, "VACUUM;", err) && checkpoint(db, err);
}
