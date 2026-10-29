// Schemas of remote databases and resumable cloud migrations (soju-rs
// database/sqlite/mod.rs open_remote_controller and sqlite/turso.rs)
#include <string.h>

#include "base/alloc.h"
#include "db/hrana.h"
#include "db/internal.h"
#include "db/turso.h"

static const char CONTROLLER_SCHEMA[] = {
#embed "turso/controller_schema.sql"
	, '\0'};
static const char TENANT_USER_MIGRATION[] = {
#embed "turso/tenant_user_migration.sql"
	, '\0'};

static bool init_schema_version(struct db_conn *c, struct err **err) {
	if (!db_exec(c,
		     "CREATE TABLE IF NOT EXISTS SojuSchemaVersion (\n"
		     "            id INTEGER PRIMARY KEY CHECK(id=0), version INTEGER NOT NULL)",
		     err)) {
		return false;
	}
	int64_t rows = 0;
	if (!db_query_i64(c, "SELECT count(*) FROM SojuSchemaVersion", &rows, err)) {
		return false;
	}
	if (rows > 0) {
		return true;
	}
	// Read legacy installations without writing the hosted read-only
	// pragma. New databases have user_version=0.
	int64_t legacy = 0;
	return db_query_i64(c, "PRAGMA user_version", &legacy, err) &&
	       db_exec_i64(c,
			   "INSERT INTO SojuSchemaVersion(id, version) VALUES(0, ?) ON CONFLICT "
			   "DO NOTHING",
			   legacy, err);
}

bool turso_init_controller(struct db_conn *c, struct err **err) {
	if (!init_schema_version(c, err) || !db_begin(c, err)) {
		return false;
	}
	int64_t version = 0;
	bool ok = db_conn_schema_version(c, &version, err);
	if (ok && version == 0) {
		ok = db_exec(c, CONTROLLER_SCHEMA, err);
	} else if (ok && version != 1 && version != 2) {
		ok = err_set(err, 0, "unsupported Turso controller schema version %lld",
			     (long long)version);
	}
	ok = ok &&
	     db_exec(c,
		     "CREATE INDEX IF NOT EXISTS DeviceCertificate_user_index ON "
		     "DeviceCertificate(user);",
		     err) &&
	     (version >= 2 || db_conn_set_schema_version(c, 2, err)) && db_commit(c, err);
	if (!ok) {
		db_rollback(c);
	}
	return ok;
}

static bool query_bool(struct db_conn *c, const char *sql, bool *out, struct err **err) {
	int64_t v = 0;
	if (!db_query_i64(c, sql, &v, err)) {
		return false;
	}
	*out = v != 0;
	return true;
}

static bool has_user_table(struct db_conn *c, bool *out, struct err **err) {
	return query_bool(
		c, "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='User')",
		out, err);
}

// The database belongs to this controller and user (TursoOwner)
static bool check_owner(struct db_conn *c, const char *controller, int64_t owner,
			struct err **err) {
	if (!db_begin(c, err)) {
		return false;
	}
	bool legacy = false, ok = has_user_table(c, &legacy, err);
	if (ok && legacy) {
		struct db_stmt *st = db_prepare(c,
						"SELECT EXISTS(SELECT 1 FROM User WHERE id!=? OR "
						"username!=? OR password IS "
						"NOT NULL)",
						err);
		ok = st != nullptr;
		if (ok) {
			AUTOFREE char *name = xasprintf("user-%lld", (long long)owner);
			bind_pos_i64(st, 1, owner);
			bind_pos_text(st, 2, name);
			ok = db_step(st) == SQLITE_ROW || db_sql_err(c, err);
			if (ok && col_bool(st, 0)) {
				ok = err_set(err, 0,
					     "Turso user database belongs to a different account");
			}
			db_finalize(st);
		}
	}
	ok = ok && db_exec(c,
			   "CREATE TABLE IF NOT EXISTS TursoOwner(id INTEGER PRIMARY KEY "
			   "CHECK(id=0), controller TEXT NOT NULL, user INTEGER NOT NULL);\n"
			   "            CREATE UNIQUE INDEX IF NOT EXISTS TursoOwner_user_index ON "
			   "TursoOwner(user);",
			   err);
	struct db_stmt *st =
		ok ? db_prepare(c, "SELECT controller,user FROM TursoOwner WHERE id=0", err)
		   : nullptr;
	ok = st != nullptr;
	int rc = ok ? db_step(st) : SQLITE_ERROR;
	if (ok && rc == SQLITE_ROW) {
		AUTOFREE char *saved = col_lossy(st, 0);
		if (!cstr_eq(saved, controller) || col_i64(st, 1) != owner) {
			ok = err_set(err, 0,
				     "Turso user database has a different controller or owner");
		}
	} else if (ok && rc == SQLITE_DONE) {
		int64_t version = 0;
		ok = db_conn_schema_version(c, &version, err);
		if (ok && !legacy && version != 0) {
			ok = err_set(err, 0, "Turso user database is missing its owner");
		}
		db_finalize(st);
		st = ok ? db_prepare(c, "INSERT INTO TursoOwner(id,controller,user) VALUES(0,?,?)",
				     err)
			: nullptr;
		ok = st != nullptr;
		if (ok) {
			bind_pos_text(st, 1, controller);
			bind_pos_i64(st, 2, owner);
			ok = db_step_done(st, err);
		}
	} else if (ok) {
		ok = db_sql_err(c, err);
	}
	db_finalize(st);
	if (!ok || !db_commit(c, err)) {
		db_rollback(c);
		return false;
	}
	return true;
}

// The SQL of a sqlite_master entry, without whitespace
static bool schema_sql(struct db_conn *c, const char *sql, struct strbuf *out, struct err **err) {
	struct db_stmt *st = db_prepare(c, sql, err);
	if (st == nullptr) {
		return false;
	}
	int rc = db_step(st);
	bool ok = rc == SQLITE_ROW || db_sql_err(c, err);
	if (rc == SQLITE_ROW) {
		struct str s = col_bytes(st, 0);
		for (size_t i = 0; i < s.len; i++) {
			if (s.p[i] != ' ' && s.p[i] != '\t' && s.p[i] != '\n' && s.p[i] != '\r') {
				strbuf_append_char(out, s.p[i]);
			}
		}
	} else if (rc == SQLITE_DONE) {
		ok = err_set(err, 0, "Query returned no rows");
	}
	db_finalize(st);
	return ok;
}

// The triggers of the FTS schema, from its first CREATE TRIGGER
static const char *fts_triggers(void) {
	return strstr(db_fts_schema(), "CREATE TRIGGER");
}

static bool is_timeout(const struct err *e) {
	const char *m = err_msg(e);
	return strstr(m, "TIMEOUT") != nullptr || strstr(m, "timed out") != nullptr ||
	       strstr(m, "HTTP 504") != nullptr;
}

// A statement of an atomic batch with named integer and text parameters
static void batch_stmt(struct hrana_stmt *s, const char *sql) {
	*s = (struct hrana_stmt){.sql = xstrdup(sql)};
}

static void batch_i64(struct hrana_stmt *s, const char *name, int64_t v) {
	vec_push(&s->binds,
		 ((struct db_bind){.name = xstrdup(name), .v = {.type = DB_INTEGER, .i = v}}));
}

static void batch_text(struct hrana_stmt *s, const char *name, const char *v) {
	struct db_bind b = {.name = xstrdup(name), .v = {.type = DB_TEXT}};
	strbuf_append_cstr(&b.v.b, v);
	vec_push(&s->binds, b);
}

// Copies a rowid Message table into the clustered WITHOUT ROWID layout in
// bounded transactions. Message IDs are kept, so the search index stays
// valid. Its cursor commits with each chunk, so a restart or an ambiguous
// response can't copy a row twice.
static bool rebuild_messages(struct db_conn *c, struct err **err) {
	AUTO_STRBUF struct strbuf sql = STRBUF_INIT;
	if (!schema_sql(c, "SELECT sql FROM sqlite_master WHERE type='table' AND name='Message'",
			&sql, err)) {
		return false;
	}
	for (char *p = sql.buf; p != nullptr && *p != '\0'; p++) {
		*p = (char)((*p >= 'a' && *p <= 'z') ? *p - 'a' + 'A' : *p);
	}
	if (strstr(strbuf_cstr(&sql), "WITHOUTROWID") != nullptr) {
		return true;
	}
	const char *schema = db_tenant_schema();
	const char *start = strstr(schema, "CREATE TABLE Message (");
	const char *end = start != nullptr ? strstr(start, ") WITHOUT ROWID;") : nullptr;
	if (end == nullptr) {
		return err_set(err, 0, "missing tenant Message schema");
	}
	start += strlen("CREATE TABLE Message (");
	// Reservations take over from max(id), which the clustered table can't
	// seek. Older releases inserted without updating this row.
	AUTOFREE char *setup = xasprintf(
		"CREATE TABLE IF NOT EXISTS MessageNext (%.*s) WITHOUT ROWID;\n"
		"CREATE UNIQUE INDEX IF NOT EXISTS MessageTextIdIndex ON MessageNext(target, id) "
		"WHERE text IS NOT NULL;\n"
		"CREATE TABLE IF NOT EXISTS TursoMessageRebuild(id INTEGER PRIMARY KEY "
		"CHECK(id=0), message INTEGER NOT NULL);\n"
		"INSERT INTO TursoMessageRebuild(id,message) VALUES(0,0) ON CONFLICT DO NOTHING;\n"
		"INSERT INTO MessageIdHighWater(id,last_id) SELECT 0,id FROM Message WHERE true "
		"ORDER BY id DESC LIMIT 1\n"
		"    ON CONFLICT(id) DO UPDATE SET last_id=excluded.last_id WHERE "
		"last_id<excluded.last_id;",
		(int)(end - start), start);
	if (!db_exec(c, setup, err)) {
		return false;
	}
	int64_t page = 1000;
	for (;;) {
		int64_t cursor = 0, last = 0;
		if (!db_query_i64(c, "SELECT message FROM TursoMessageRebuild WHERE id=0", &cursor,
				  err)) {
			return false;
		}
		struct db_stmt *st = db_prepare(c,
						"SELECT max(id) FROM (SELECT id FROM Message WHERE "
						"id>? ORDER BY id LIMIT ?)",
						err);
		if (st == nullptr) {
			return false;
		}
		bind_pos_i64(st, 1, cursor);
		bind_pos_i64(st, 2, page);
		int rc = db_step(st);
		bool none = rc == SQLITE_ROW && col_type(st, 0) == SQLITE_NULL;
		last = rc == SQLITE_ROW ? col_i64(st, 0) : 0;
		db_finalize(st);
		if (rc != SQLITE_ROW) {
			return db_sql_err(c, err);
		}
		if (none) {
			break;
		}
		struct hrana_stmt steps[2];
		batch_stmt(&steps[0],
			   "INSERT INTO MessageNext(target,time,id,raw,sender,text)\n"
			   "    SELECT target,time,id,raw,sender,text FROM Message WHERE "
			   "id>:cursor AND id<=:end\n"
			   "    AND EXISTS(SELECT 1 FROM TursoMessageRebuild WHERE id=0 AND "
			   "message=:cursor)");
		batch_stmt(&steps[1], "UPDATE TursoMessageRebuild SET message=:end WHERE id=0 AND "
				      "message=:cursor");
		for (size_t i = 0; i < 2; i++) {
			batch_i64(&steps[i], ":cursor", cursor);
			batch_i64(&steps[i], ":end", last);
		}
		struct err *e = nullptr;
		bool ok = hrana_atomic_batch(c->hrana, steps, 2, &e);
		hrana_stmt_free(&steps[0]);
		hrana_stmt_free(&steps[1]);
		if (!ok) {
			if (page > 1 && is_timeout(e)) {
				// Read the durable cursor before retrying
				err_free(e);
				page = page / 2 > 1 ? page / 2 : 1;
				continue;
			}
			if (err != nullptr) {
				*err = e;
			} else {
				err_free(e);
			}
			return false;
		}
	}
	// Nothing references Message. Dropping it also drops its indexes and
	// search triggers; the search index keeps the same row IDs.
	AUTOFREE char *swap = xasprintf("DROP TABLE Message; ALTER TABLE MessageNext RENAME TO "
					"Message;\nDROP TABLE TursoMessageRebuild; %s",
					fts_triggers());
	if (!db_begin(c, err)) {
		return false;
	}
	if (!db_exec(c, swap, err) || !db_commit(c, err)) {
		db_rollback(c);
		return false;
	}
	return true;
}

struct search_cursor {
	int64_t target, message;
	char *time;
};

static bool read_cursor(struct db_conn *c, struct search_cursor *cur, struct err **err) {
	struct db_stmt *st = db_prepare(
		c, "SELECT target,time,message FROM TursoSearchMigration WHERE id=0", err);
	if (st == nullptr) {
		return false;
	}
	bool ok = db_step(st) == SQLITE_ROW || db_sql_err(c, err);
	if (ok) {
		*cur = (struct search_cursor){col_i64(st, 0), col_i64(st, 2), col_lossy(st, 1)};
	}
	db_finalize(st);
	return ok;
}

static void cursor_binds(struct hrana_stmt *s, const struct search_cursor *cur) {
	batch_i64(s, ":target", cur->target);
	batch_text(s, ":time", cur->time);
	batch_i64(s, ":id", cur->message);
}

// One chunk of the search migration: false on error, *done when the
// current target has no more messages to index
static bool search_chunk(struct db_conn *c, const struct search_cursor *cur, int64_t *page,
			 bool *done, struct err **err) {
	*done = false;
	struct db_stmt *st = db_prepare(
		c,
		"SELECT time,id FROM Message WHERE target=:target AND text IS NOT NULL\n"
		"    AND time>=:time AND (time>:time OR id>:id) ORDER BY time,id LIMIT :limit",
		err);
	if (st == nullptr) {
		return false;
	}
	bind_i64(st, ":target", cur->target);
	bind_text(st, ":time", cur->time);
	bind_i64(st, ":id", cur->message);
	bind_i64(st, ":limit", *page);
	AUTOFREE char *end_time = nullptr;
	int64_t end_id = 0;
	int rc;
	while ((rc = db_step(st)) == SQLITE_ROW) {
		xfree(end_time);
		end_time = col_lossy(st, 0);
		end_id = col_i64(st, 1);
	}
	db_finalize(st);
	if (rc != SQLITE_DONE) {
		return db_sql_err(c, err);
	}
	if (end_time == nullptr) {
		*done = true;
		return true;
	}
	struct hrana_stmt steps[2];
	batch_stmt(&steps[0], "INSERT INTO MessageFTSNext(rowid,text) SELECT id,text FROM Message\n"
			      "    WHERE target=:target AND text IS NOT NULL AND time>=:time\n"
			      "    AND (time>:time OR id>:id) AND time<=:end_time\n"
			      "    AND (time<:end_time OR id<=:end_id)\n"
			      "    AND EXISTS(SELECT 1 FROM TursoSearchMigration WHERE id=0\n"
			      "        AND target=:target AND time=:time AND message=:id)");
	batch_stmt(&steps[1], "UPDATE TursoSearchMigration SET time=:end_time,message=:end_id "
			      "WHERE id=0\n"
			      "    AND target=:target AND time=:time AND message=:id");
	for (size_t i = 0; i < 2; i++) {
		cursor_binds(&steps[i], cur);
		batch_text(&steps[i], ":end_time", end_time);
		batch_i64(&steps[i], ":end_id", end_id);
	}
	struct err *e = nullptr;
	bool ok = hrana_atomic_batch(c->hrana, steps, 2, &e);
	hrana_stmt_free(&steps[0]);
	hrana_stmt_free(&steps[1]);
	if (!ok && *page > 1 && is_timeout(e)) {
		// Read the durable cursor before retrying
		err_free(e);
		*page = *page / 2 > 1 ? *page / 2 : 1;
		return true;
	}
	if (!ok) {
		if (err != nullptr) {
			*err = e;
		} else {
			err_free(e);
		}
	}
	return ok;
}

// Builds the sparse search index in bounded transactions; its cursor
// commits with each chunk, so a restart or an ambiguous response can't
// index a message twice
static bool migrate_search(struct db_conn *c, struct err **err) {
	AUTO_STRBUF struct strbuf sql = STRBUF_INIT;
	if (!schema_sql(c, "SELECT sql FROM sqlite_master WHERE name='MessageFTS'", &sql, err)) {
		return false;
	}
	if (strstr(strbuf_cstr(&sql), "columnsize=0") != nullptr) {
		return true;
	}
	if (!db_exec(c,
		     "CREATE VIRTUAL TABLE IF NOT EXISTS MessageFTSNext USING "
		     "fts5(text,content=Message,content_rowid=id,columnsize=0);\n"
		     "CREATE TABLE IF NOT EXISTS TursoSearchMigration(id INTEGER PRIMARY KEY "
		     "CHECK(id=0), target INTEGER NOT NULL,time TEXT NOT NULL,message INTEGER NOT "
		     "NULL);\n"
		     "INSERT INTO TursoSearchMigration(id,target,time,message) VALUES(0,0,'',0) ON "
		     "CONFLICT DO NOTHING;",
		     err)) {
		return false;
	}
	int64_t page = 200;
	for (;;) {
		struct search_cursor cur;
		if (!read_cursor(c, &cur, err)) {
			return false;
		}
		bool done = false;
		bool ok = search_chunk(c, &cur, &page, &done, err);
		if (ok && done) {
			struct db_stmt *st =
				db_prepare(c,
					   "SELECT id FROM MessageTarget t WHERE id>? AND EXISTS(\n"
					   "    SELECT 1 FROM Message WHERE target=t.id AND text "
					   "IS NOT NULL)\n"
					   "    ORDER BY id LIMIT 1",
					   err);
			ok = st != nullptr;
			int rc = SQLITE_DONE;
			int64_t next = 0;
			if (ok) {
				bind_pos_i64(st, 1, cur.target);
				rc = db_step(st);
				next = rc == SQLITE_ROW ? col_i64(st, 0) : 0;
				ok = rc == SQLITE_ROW || rc == SQLITE_DONE || db_sql_err(c, err);
				db_finalize(st);
			}
			if (ok && rc == SQLITE_ROW) {
				st = db_prepare(
					c,
					"UPDATE TursoSearchMigration SET "
					"target=:next,time='',message=0 WHERE id=0\n"
					"    AND target=:target AND time=:time AND message=:id",
					err);
				ok = st != nullptr;
				if (ok) {
					bind_i64(st, ":next", next);
					bind_i64(st, ":target", cur.target);
					bind_text(st, ":time", cur.time);
					bind_i64(st, ":id", cur.message);
					ok = db_step_done(st, err);
					db_finalize(st);
				}
				done = false;
			}
		}
		xfree(cur.time);
		if (!ok) {
			return false;
		}
		if (done) {
			break;
		}
	}
	// The replacement virtual table already exists; install only triggers
	AUTOFREE char *swap = xasprintf(
		"DROP TRIGGER MessageFTSInsert; DROP TRIGGER MessageFTSDelete; DROP TRIGGER "
		"MessageFTSUpdate;\n"
		"DROP TABLE MessageFTS; ALTER TABLE MessageFTSNext RENAME TO MessageFTS; DROP "
		"TABLE TursoSearchMigration;\n%s",
		fts_triggers());
	if (!db_begin(c, err)) {
		return false;
	}
	if (!db_exec(c, swap, err) || !db_commit(c, err)) {
		db_rollback(c);
		return false;
	}
	return true;
}

static bool exists(struct db_conn *c, const char *sql, bool *out, struct err **err) {
	return query_bool(c, sql, out, err);
}

// Older tenants had a User table: authentication data moved to the
// controller, the owner to TursoOwner
static bool remove_user(struct db_conn *c, struct err **err) {
	bool legacy = false;
	if (!has_user_table(c, &legacy, err)) {
		return false;
	}
	if (!legacy) {
		return true;
	}
	if (!db_exec(c, "PRAGMA foreign_keys = 0", err) || !db_begin(c, err)) {
		return false;
	}
	bool ok = true, found = false;
	static const char *const auth[] = {"DeviceCertificate", "WebPushConfig"};
	for (size_t i = 0; ok && i < 2; i++) {
		AUTOFREE char *sql = xasprintf("SELECT EXISTS(SELECT 1 FROM %s)", auth[i]);
		ok = exists(c, sql, &found, err);
		if (ok && found) {
			ok = err_set(
				err, 0,
				"unexpected authentication data in tenant %s; migrate it to the "
				"controller before removing User",
				auth[i]);
		}
	}
	static const char *const owned[] = {"Network", "WebPushSubscription"};
	for (size_t i = 0; ok && i < 2; i++) {
		AUTOFREE char *sql = xasprintf("SELECT EXISTS(SELECT 1 FROM %s WHERE user!=(SELECT "
					       "user FROM TursoOwner WHERE id=0))",
					       owned[i]);
		ok = exists(c, sql, &found, err);
		if (ok && found) {
			ok = err_set(err, 0, "tenant %s belongs to a different owner", owned[i]);
		}
	}
	ok = ok && db_exec(c, TENANT_USER_MIGRATION, err);
	// Only these tables were rebuilt: a database-wide check would scan the
	// whole message history
	for (size_t i = 0; ok && i < 2; i++) {
		AUTOFREE char *sql = xasprintf("PRAGMA foreign_key_check(%s)", owned[i]);
		struct db_stmt *st = db_prepare(c, sql, err);
		ok = st != nullptr;
		if (ok) {
			int rc = db_step(st);
			ok = rc == SQLITE_DONE ||
			     (rc == SQLITE_ROW ? err_set(err, 0,
							 "tenant migration would leave a "
							 "foreign-key violation")
					       : db_sql_err(c, err));
			db_finalize(st);
		}
	}
	ok = ok && db_commit(c, err);
	if (!ok) {
		db_rollback(c);
	}
	return db_exec(c, "PRAGMA foreign_keys = 1", ok ? err : nullptr) && ok;
}

// "<prefix>; restart to resume: <e>"
static bool resumable(const char *what, struct err *e, struct err **err) {
	err_set(err, 0, "%s; restart to resume: %s", what, err_msg(e));
	err_free(e);
	return false;
}

bool turso_init_tenant(struct db_conn *c, const char *controller, int64_t owner, struct err **err) {
	if (!init_schema_version(c, err) || !check_owner(c, controller, owner, err) ||
	    !db_conn_upgrade(c, err) ||
	    !db_exec(c,
		     "CREATE TABLE IF NOT EXISTS MessageIdHighWater (id INTEGER PRIMARY KEY "
		     "CHECK(id=0), last_id INTEGER NOT NULL);",
		     err)) {
		return false;
	}
	struct err *e = nullptr;
	if (!rebuild_messages(c, &e)) {
		return resumable("rebuild Turso message table", e, err);
	}
	if (!migrate_search(c, &e)) {
		return resumable("migrate Turso search index", e, err);
	}
	return remove_user(c, err);
}
