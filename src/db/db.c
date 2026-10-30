#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "base/alloc.h"
#include "base/utf8.h"
#include "db/internal.h"
#include "db/turso.h"

static const char SCHEMA[] = {
#embed "sqlite_schema.sql"
	, '\0'};
static const char FTS_SCHEMA[] = {
#embed "message_fts.sql"
	, '\0'};
static const char TENANT_SCHEMA[] = {
#embed "turso/tenant_schema.sql"
	, '\0'};

const char *db_fts_schema(void) {
	return FTS_SCHEMA;
}

const char *db_tenant_schema(void) {
	return TENANT_SCHEMA;
}

// SQLite schema migrations, including upstream soju's. Migration #0 is
// reserved for schema initialization.
static const char *const MIGRATIONS[] = {
#include "db/migrations.inc"
};

static constexpr int64_t SCHEMA_VERSION = sizeof(MIGRATIONS) / sizeof(MIGRATIONS[0]);
static_assert(SCHEMA_VERSION == 35);

// The migration clustering the local Message table. Turso tenants skip it:
// turso.c clusters them in resumable chunks.
static constexpr int64_t LOCAL_MESSAGE_CLUSTER = 33;

int64_t db_schema_version(void) {
	return SCHEMA_VERSION;
}

const char *db_migration(size_t i) {
	return i < (size_t)SCHEMA_VERSION ? MIGRATIONS[i] : nullptr;
}

// Turso can't write PRAGMA user_version: remote databases keep their
// version in a table
bool db_conn_schema_version(struct db_conn *c, int64_t *out, struct err **err) {
	return db_query_i64(c,
			    c->remote ? "SELECT version FROM SojuSchemaVersion WHERE id=0"
				      : "PRAGMA user_version",
			    out, err);
}

bool db_conn_set_schema_version(struct db_conn *c, int64_t v, struct err **err) {
	if (c->remote) {
		return db_exec_i64(c, "UPDATE SojuSchemaVersion SET version=? WHERE id=0", v, err);
	}
	char sql[64];
	snprintf(sql, sizeof(sql), "PRAGMA user_version = %lld", (long long)v);
	return db_exec(c, sql, err);
}

// Turso's hosted SQL allowlist rejects ANALYZE. The partial index itself is
// supported. The Message rebuild that follows replaces the timestamp
// indexes: don't write an index entry for every message first.
static char *remote_migration(const char *m) {
	struct strbuf sb = STRBUF_INIT;
	static const char *const drop[] = {
		"ANALYZE MessageTextIndex;",
		"CREATE INDEX IF NOT EXISTS MessageEventIndex ON Message(target, time) WHERE text "
		"IS "
		"NULL;",
	};
	struct str rest = str_from(m);
	for (;;) {
		ptrdiff_t at = -1;
		size_t which = 0;
		for (size_t i = 0; i < 2; i++) {
			ptrdiff_t p = str_find(rest, str_from(drop[i]));
			if (p >= 0 && (at < 0 || p < at)) {
				at = p;
				which = i;
			}
		}
		if (at < 0) {
			strbuf_append_str(&sb, rest);
			break;
		}
		strbuf_append(&sb, rest.p, (size_t)at);
		rest = str_slice(rest, (size_t)at + strlen(drop[which]), rest.len);
	}
	return strbuf_take(&sb);
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
	// Another process may have initialized the database while BEGIN waited
	if (c->remote && !db_conn_schema_version(c, &version, err)) {
		goto fail;
	}
	if (version > SCHEMA_VERSION) {
		err_set(err, 0, "unsupported database schema version %lld", (long long)version);
		goto fail;
	}
	if (version == SCHEMA_VERSION) {
		if (!db_commit(c, err)) {
			goto fail;
		}
		return true;
	}
	if (version == 0) {
		if (!db_exec(c, c->remote ? TENANT_SCHEMA : SCHEMA, &e)) {
			wrap(err, e, "failed to initialize schema");
			goto fail;
		}
		if (!db_exec(c, FTS_SCHEMA, err)) {
			goto fail;
		}
	} else {
		for (int64_t i = version; i < SCHEMA_VERSION; i++) {
			if (c->remote && i == LOCAL_MESSAGE_CLUSTER) {
				continue;
			}
			AUTOFREE char *migration = c->remote ? remote_migration(MIGRATIONS[i])
							     : xstrdup(MIGRATIONS[i]);
			if (!db_exec(c, migration, &e)) {
				char prefix[64];
				snprintf(prefix, sizeof(prefix),
					 "failed to execute migration #%lld", (long long)i);
				wrap(err, e, prefix);
				goto fail;
			}
		}
		if (!c->remote && version < 32 &&
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
	if (!db_conn_set_schema_version(c, SCHEMA_VERSION, &e)) {
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
	return db_conn_upgrade(db->account, err);
}

bool db_conn_upgrade(struct db_conn *c, struct err **err) {
	int64_t version;
	if (!db_conn_schema_version(c, &version, err)) {
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
	struct db_conn *c = db->account;
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
	// Local high-water marks keep asynchronous actor IDs from being reused
	// after deletion. They do not change upstream user_version. Message IDs
	// are reserved from MessageIdHighWater, part of the schema.
	return db_exec(
		c,
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
	db_conn_init_local(&db->local, conn);
	db->account = &db->local;
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
	if (db->cloud != nullptr) {
		turso_close(db->cloud);
	} else {
		db_conn_finish(&db->local);
	}
	xfree(db);
}

sqlite3 *db_handle(struct db *db) {
	return db->local.sqlite;
}

// One database's counts; tenants have no User table
static bool conn_stats(struct db_conn *c, struct db_stats *out, struct err **err) {
	struct db_stmt *st = db_prepare(
		c,
		c->remote ? "SELECT\n                0 AS users,\n"
			    "                (SELECT COUNT(*) FROM Network) AS networks,\n"
			    "                (SELECT COUNT(*) FROM Channel) AS channels"
			  : "SELECT\n                (SELECT COUNT(*) FROM User) AS users,\n"
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

bool db_stats(struct db *db, struct db_stats *out, struct err **err) {
	if (db->cloud == nullptr) {
		return conn_stats(db->account, out, err);
	}
	db_user_vec users = {0};
	if (!db_list_users(db, &users, err)) {
		return false;
	}
	*out = (struct db_stats){.users = (int64_t)users.len};
	db_user_vec_free(&users);
	for (size_t i = 0; i < db->cloud->users.len; i++) {
		struct db_stats s;
		if (!conn_stats(&db->cloud->users.data[i]->conn, &s, err)) {
			return false;
		}
		out->networks += s.networks;
		out->channels += s.channels;
	}
	return true;
}

// Turso doesn't need (nor allow) file maintenance
bool db_free_page_ratio(struct db *db, double *out, struct err **err) {
	*out = 0.0;
	if (db->cloud != nullptr) {
		return true;
	}
	int64_t pages, free_pages;
	if (!db_query_i64(db->account, "PRAGMA page_count", &pages, err) ||
	    !db_query_i64(db->account, "PRAGMA freelist_count", &free_pages, err)) {
		return false;
	}
	*out = pages > 0 ? (double)free_pages / (double)pages : 0.0;
	return true;
}

static bool checkpoint(struct db *db, struct err **err) {
	int64_t ignored;
	return db_query_i64(db->account, "PRAGMA wal_checkpoint(TRUNCATE)", &ignored, err);
}

bool db_optimize(struct db *db, struct err **err) {
	static const char optimize[] = "INSERT INTO MessageFTS(MessageFTS) VALUES ('optimize');";
	if (db->cloud != nullptr) {
		for (size_t i = 0; i < db->cloud->users.len; i++) {
			if (!db_exec(&db->cloud->users.data[i]->conn, optimize, err)) {
				return false;
			}
		}
		return true;
	}
	return db_exec(db->account, optimize, err) && checkpoint(db, err);
}

bool db_shrink_memory(struct db *db, struct err **err) {
	return db->cloud != nullptr || db_exec(db->account, "PRAGMA shrink_memory;", err);
}

bool db_vacuum(struct db *db, struct err **err) {
	// VACUUM goes through the WAL, which keeps its size on disk until
	// truncated by a checkpoint
	return db->cloud != nullptr ||
	       (db_exec(db->account, "VACUUM;", err) && checkpoint(db, err));
}
