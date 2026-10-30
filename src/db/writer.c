#include "db/writer.h"

#include <openssl/rand.h>
#include <stdlib.h>

#include "base/alloc.h"
#include "db/hrana.h"
#include "db/internal.h"
#include "db/turso.h"

// Successful chunks after which a lowered line limit doubles again
static constexpr size_t MAX_LINES_RECOVERY = 8;
// Rows per INSERT: below even the older 999-parameter limit of SQLite
static constexpr size_t ROWS_PER_INSERT = 100;
static constexpr size_t TARGET_CACHE_MAX = 4096;

bool db_message_insert_init(struct db_message_insert *m, int64_t id, int64_t network,
			    const char *target, const struct irc_msg *msg, struct err **err) {
	*m = (struct db_message_insert){.id = id, .network = network};
	if (!message_time_text(msg, &m->time, &m->text, err)) {
		return false;
	}
	m->target = xstrdup(target);
	m->raw = irc_msg_to_string(msg);
	m->sender = xstrdup(irc_msg_prefix_name(msg));
	return true;
}

void db_message_insert_free(struct db_message_insert *m) {
	xfree(m->target);
	xfree(m->raw);
	xfree(m->time);
	xfree(m->sender);
	xfree(m->text);
	*m = (struct db_message_insert){0};
}

size_t db_message_insert_bytes(const struct db_message_insert *m) {
	return strlen(m->raw) + strlen(m->target) + (m->text != nullptr ? strlen(m->text) : 0);
}

struct target_id {
	int64_t network;
	char *target;
	int64_t id;
	bool create;
	bool gone; // the network no longer exists (this batch only)
};

struct db_message_writer {
	struct db_conn conn;
	VEC(struct target_id) targets;
	size_t max_lines; // 0: no limit
	size_t successes;
};

static void clear_targets(struct db_message_writer *w) {
	for (size_t i = 0; i < w->targets.len; i++) {
		xfree(w->targets.data[i].target);
	}
	w->targets.len = 0;
}

struct db_message_writer *db_message_writer_new(struct db *db, int64_t user_id,
						 struct err **err) {
	if (db->cloud == nullptr) {
		err_set(err, 0, "background message writer requires Turso");
		return nullptr;
	}
	struct hrana *h = turso_fork_user(db->cloud, user_id, err);
	if (h == nullptr) {
		return nullptr;
	}
	struct db_message_writer *w = xcalloc(1, sizeof(*w));
	db_conn_init_remote(&w->conn, h);
	return w;
}

void db_message_writer_free(struct db_message_writer *w) {
	if (w == nullptr) {
		return;
	}
	clear_targets(w);
	vec_free(&w->targets);
	db_conn_finish(&w->conn);
	xfree(w);
}

bool db_message_writer_reserve(struct db_message_writer *w, int64_t count, int64_t *first,
			       int64_t *last, struct err **err) {
	if (count <= 0) {
		return err_set(err, 0, "empty message ID reservation");
	}
	// Every tenant ID is reserved here: the clustered Message table can't
	// seek max(id)
	struct db_stmt *st = db_prepare(
		&w->conn,
		"INSERT INTO MessageIdHighWater(id, last_id)\n"
		"             SELECT 0, coalesce((SELECT last_id FROM MessageIdHighWater WHERE "
		"id=0), 0) + :count\n"
		"             WHERE coalesce((SELECT last_id FROM MessageIdHighWater WHERE id=0), "
		"0) <= :maximum\n"
		"             ON CONFLICT(id) DO UPDATE SET last_id=excluded.last_id\n"
		"             RETURNING last_id",
		err);
	if (st == nullptr) {
		return false;
	}
	bind_i64(st, ":count", count);
	bind_i64(st, ":maximum", INT64_MAX - count);
	int rc = db_step(st);
	bool ok = rc == SQLITE_ROW;
	if (ok) {
		*last = col_i64(st, 0);
		*first = *last - count + 1;
	} else if (rc == SQLITE_DONE) {
		err_set(err, 0, "Query returned no rows");
	} else {
		db_sql_err(&w->conn, err);
	}
	db_finalize(st);
	return ok;
}

static struct target_id *find_target(struct db_message_writer *w, int64_t network,
				     const char *target) {
	for (size_t i = 0; i < w->targets.len; i++) {
		struct target_id *t = &w->targets.data[i];
		if (t->network == network && cstr_eq(t->target, target)) {
			return t;
		}
	}
	return nullptr;
}

// The index of a message's target: cached, or looked up (created by the
// batch if missing); -1 on error
static ptrdiff_t resolve(struct db_message_writer *w, int64_t network, const char *target,
			 struct err **err) {
	struct target_id *t = find_target(w, network, target);
	if (t != nullptr) {
		return t - w->targets.data;
	}
	struct db_stmt *st = db_prepare(&w->conn,
					"SELECT t.id FROM Network n LEFT JOIN MessageTarget t ON "
					"t.network=n.id AND t.target=:target WHERE n.id=:network",
					err);
	if (st == nullptr) {
		return -1;
	}
	bind_i64(st, ":network", network);
	bind_text(st, ":target", target);
	int rc = db_step(st);
	struct target_id n = {.network = network, .target = xstrdup(target)};
	if (rc == SQLITE_DONE) {
		n.gone = true;
	} else if (rc == SQLITE_ROW && col_type(st, 0) != SQLITE_NULL) {
		n.id = col_i64(st, 0);
	} else if (rc == SQLITE_ROW) {
		uint64_t r = 0;
		if (RAND_bytes((unsigned char *)&r, sizeof(r)) != 1) {
			abort();
		}
		n.id = (int64_t)(r & ((uint64_t)INT64_MAX >> 1U));
		n.id = n.id > 0 ? n.id : 1;
		n.create = true;
	} else {
		db_sql_err(&w->conn, err);
		db_finalize(st);
		xfree(n.target);
		return -1;
	}
	db_finalize(st);
	vec_push(&w->targets, n);
	return (ptrdiff_t)w->targets.len - 1;
}

static void bind_value(struct hrana_stmt *s, int index, enum db_value_type type, const char *p) {
	struct db_bind b = {.index = index, .v = {.type = p != nullptr ? type : DB_NULL}};
	if (p != nullptr) {
		strbuf_append_cstr(&b.v.b, p);
	}
	vec_push(&s->binds, b);
}

static void free_stmts(struct hrana_stmt *stmts, size_t n) {
	for (size_t i = 0; i < n; i++) {
		hrana_stmt_free(&stmts[i]);
	}
	xfree(stmts);
}

// soju-rs WriterConnection::write: one atomic batch
static bool write_batch(struct db_message_writer *w, const struct db_message_insert *msgs,
			size_t n, struct err **err) {
	if (w->targets.len > TARGET_CACHE_MAX) {
		clear_targets(w);
	}
	VEC(struct hrana_stmt) stmts = {0};
	VEC(size_t) used = {0};
	for (size_t i = 0; i < n; i++) {
		ptrdiff_t index = resolve(w, msgs[i].network, msgs[i].target, err);
		if (index < 0) {
			goto fail;
		}
		bool seen = false;
		for (size_t j = 0; j < used.len; j++) {
			seen = seen || used.data[j] == (size_t)index;
		}
		struct target_id *t = &w->targets.data[index];
		if (seen || t->gone) {
			continue;
		}
		vec_push(&used, (size_t)index);
		if (t->create) {
			struct hrana_stmt s = {
				.sql = xstrdup("INSERT INTO MessageTarget(id,network,target) SELECT "
					       ":id,:network,:target\n"
					       "     WHERE EXISTS(SELECT 1 FROM Network WHERE "
					       "id=:network)\n"
					       "     ON CONFLICT(network,target) DO NOTHING")};
			vec_push(&s.binds, ((struct db_bind){.name = xstrdup(":id"),
							     .v = {.type = DB_INTEGER, .i = t->id}}));
			vec_push(&s.binds,
				 ((struct db_bind){.name = xstrdup(":network"),
						   .v = {.type = DB_INTEGER, .i = t->network}}));
			struct db_bind tb = {.name = xstrdup(":target"), .v = {.type = DB_TEXT}};
			strbuf_append_cstr(&tb.v.b, t->target);
			vec_push(&s.binds, tb);
			vec_push(&stmts, s);
		}
	}
	// The rows of networks that still exist, ROWS_PER_INSERT per statement.
	// An identical retry performs no UPDATE or FTS writes; a different text
	// row with the same target and ID violates NOT NULL and aborts.
	struct hrana_stmt cur = {0};
	size_t rows = 0;
	for (size_t i = 0; i <= n; i++) {
		struct target_id *t = i < n ? find_target(w, msgs[i].network, msgs[i].target) : nullptr;
		if (i < n && (t == nullptr || t->gone)) {
			continue;
		}
		if (rows > 0 && (i == n || rows == ROWS_PER_INSERT)) {
			struct strbuf sql = STRBUF_INIT;
			strbuf_append_cstr(&sql, cur.sql);
			strbuf_append_cstr(
				&sql, " ON CONFLICT DO UPDATE SET raw=NULL WHERE NOT (\n"
				      "    Message.target=excluded.target AND Message.raw=excluded.raw "
				      "AND Message.time=excluded.time\n"
				      "    AND Message.sender=excluded.sender AND Message.text IS "
				      "excluded.text)");
			xfree(cur.sql);
			cur.sql = strbuf_take(&sql);
			vec_push(&stmts, cur);
			cur = (struct hrana_stmt){0};
			rows = 0;
		}
		if (i == n) {
			break;
		}
		struct strbuf sql = STRBUF_INIT;
		strbuf_append_cstr(&sql, rows == 0 ? "INSERT INTO Message(id,target,raw,time,sender,"
						     "text) VALUES "
						   : cur.sql);
		xfree(cur.sql);
		strbuf_printf(&sql, "%s(%lld, %lld, CAST(? AS TEXT), ?, ?, CAST(? AS TEXT))",
			      rows == 0 ? "" : ",", (long long)msgs[i].id, (long long)t->id);
		cur.sql = strbuf_take(&sql);
		int base = (int)rows * 4;
		bind_value(&cur, base + 1, DB_BLOB, msgs[i].raw);
		bind_value(&cur, base + 2, DB_TEXT, msgs[i].time);
		bind_value(&cur, base + 3, DB_TEXT, msgs[i].sender);
		bind_value(&cur, base + 4, DB_BLOB, msgs[i].text);
		rows++;
	}
	bool ok = stmts.len == 0 || hrana_atomic_batch(w->conn.hrana, stmts.data, stmts.len, err);
	if (ok) {
		for (size_t j = 0; j < used.len; j++) {
			w->targets.data[used.data[j]].create = false;
		}
	}
	// "gone" was only for this batch: the network may be resolved again
	for (size_t j = 0; j < w->targets.len;) {
		if (w->targets.data[j].gone) {
			xfree(w->targets.data[j].target);
			w->targets.data[j] = w->targets.data[--w->targets.len];
		} else {
			j++;
		}
	}
	free_stmts(stmts.data, stmts.len);
	vec_free(&used);
	return ok;
fail:
	free_stmts(stmts.data, stmts.len);
	vec_free(&used);
	return false;
}

static bool contains(const struct err *e, const char *needle) {
	return strstr(err_msg(e), needle) != nullptr;
}

static bool write_chunk(struct db_message_writer *w, const struct db_message_insert *msgs,
			size_t n, size_t *rejected, struct err **err) {
	if (n == 0) {
		return true;
	}
	// Each batch's size is checked before sending. Split on body limits
	// and processing timeouts: stable IDs make this safe even if the failed
	// request actually committed.
	struct err *e = nullptr;
	bool ok = write_batch(w, msgs, n, &e);
	if (!ok && (contains(e, "SQLITE_CONSTRAINT_FOREIGNKEY") ||
		    contains(e, "FOREIGN KEY constraint failed"))) {
		// A deletion invalidated cached target IDs: the batch rolled back.
		// Resolve again, omit removed networks, replay these idempotent rows.
		err_free(e);
		e = nullptr;
		clear_targets(w);
		ok = write_batch(w, msgs, n, &e);
	}
	if (ok) {
		// Grow back after a transient timeout lowered the limit
		if (w->max_lines != 0 && ++w->successes >= MAX_LINES_RECOVERY) {
			w->successes = 0;
			w->max_lines = w->max_lines > SIZE_MAX / 2 ? 0 : w->max_lines * 2;
		}
		return true;
	}
	bool size = contains(e, "HTTP body budget") || contains(e, "HTTP 413");
	bool timeout = contains(e, "TRANSACTION_TIMEOUT") || contains(e, "QUERY_TIMEOUT") ||
		       contains(e, "timed out") || contains(e, "HTTP 504");
	// A row the database rejects, e.g. a conflicting reserved ID, would
	// block every later message: it's isolated, the others retried
	bool reject = contains(e, "SQLITE_CONSTRAINT") || contains(e, "constraint failed");
	if (n == 1 && reject) {
		err_free(e);
		(*rejected)++;
		return true;
	}
	if (n == 1 || !(size || timeout || reject)) {
		if (err != nullptr) {
			*err = e;
		} else {
			err_free(e);
		}
		return false;
	}
	err_free(e);
	size_t middle = n / 2;
	if (timeout) {
		size_t lower = middle > 0 ? middle : 1;
		if (w->max_lines == 0 || lower < w->max_lines) {
			w->max_lines = lower;
		}
		w->successes = 0;
	}
	return write_chunk(w, msgs, middle, rejected, err) &&
	       write_chunk(w, msgs + middle, n - middle, rejected, err);
}

bool db_message_writer_write(struct db_message_writer *w, const struct db_message_insert *msgs,
			     size_t n, size_t *rejected, struct err **err) {
	*rejected = 0;
	size_t step = w->max_lines != 0 ? w->max_lines : n;
	for (size_t i = 0; i < n; i += step) {
		size_t len = n - i < step ? n - i : step;
		if (!write_chunk(w, msgs + i, len, rejected, err)) {
			return false;
		}
	}
	return true;
}

bool db_message_buffer_open(struct db **out, struct err **err) {
	if (!db_open_in_memory(out, err)) {
		return false;
	}
	if (!db_exec((*out)->account,
		     "INSERT INTO User(id, username, created_at) VALUES(1, 'pending', '')", err)) {
		db_close(*out);
		return false;
	}
	return true;
}

bool db_message_buffer_insert(struct db *buf, const struct db_message_insert *m,
			      struct err **err) {
	struct db_conn *c = buf->account;
	AUTOFREE char *addr = xasprintf("%lld", (long long)m->network);
	struct db_stmt *st = db_prepare(c,
					"INSERT INTO Network(id, user, addr) VALUES(:network, 1, "
					":addr) ON CONFLICT(id) DO NOTHING",
					err);
	if (st == nullptr) {
		return false;
	}
	bind_i64(st, ":network", m->network);
	bind_text(st, ":addr", addr);
	bool ok = db_step_done(st, err);
	db_finalize(st);
	st = ok ? db_prepare(c,
			     "INSERT INTO MessageTarget(network, target) VALUES(:network, :target) "
			     "ON CONFLICT DO NOTHING",
			     err)
		: nullptr;
	ok = st != nullptr;
	if (ok) {
		bind_i64(st, ":network", m->network);
		bind_text(st, ":target", m->target);
		ok = db_step_done(st, err);
		db_finalize(st);
	}
	st = ok ? db_prepare(c,
			     "INSERT INTO Message(id, target, raw, time, sender, text)\n"
			     "    VALUES(:id, (SELECT id FROM MessageTarget WHERE network=:network AND "
			     "target=:target), :raw, :time, :sender, :text)",
			     err)
		: nullptr;
	ok = st != nullptr;
	if (ok) {
		bind_i64(st, ":id", m->id);
		bind_i64(st, ":network", m->network);
		bind_text(st, ":target", m->target);
		bind_text(st, ":raw", m->raw);
		bind_text(st, ":time", m->time);
		bind_text(st, ":sender", m->sender);
		if (m->text != nullptr) {
			bind_text(st, ":text", m->text);
		} else {
			bind_null(st, ":text");
		}
		ok = db_step_done(st, err);
		db_finalize(st);
	}
	return ok;
}

bool db_message_buffer_remove(struct db *buf, const struct db_message_insert *msgs, size_t n,
			      struct err **err) {
	struct db_conn *c = buf->account;
	if (!db_begin(c, err)) {
		return false;
	}
	// A primary key seek in the clustered table
	struct db_stmt *st = db_prepare(c,
					"DELETE FROM Message WHERE target=(SELECT id FROM "
					"MessageTarget\n"
					"    WHERE network=:network AND target=:target) AND "
					"time=:time AND id=:id",
					err);
	bool ok = st != nullptr;
	for (size_t i = 0; ok && i < n; i++) {
		bind_i64(st, ":network", msgs[i].network);
		bind_text(st, ":target", msgs[i].target);
		bind_text(st, ":time", msgs[i].time);
		bind_i64(st, ":id", msgs[i].id);
		ok = db_step_done(st, err);
		db_clear_bindings(st);
	}
	db_finalize(st);
	// Metadata stays bounded: only targets and networks with pending
	// messages are kept
	ok = ok &&
	     db_exec(c,
		     "DELETE FROM MessageTarget WHERE id NOT IN (SELECT target FROM Message);\n"
		     "DELETE FROM Network WHERE id NOT IN (SELECT network FROM MessageTarget);",
		     err) &&
	     db_commit(c, err);
	if (!ok) {
		db_rollback(c);
	}
	return ok;
}
