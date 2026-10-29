// Message targets, logged messages and their full-text search (soju-rs
// database/sqlite/messages.rs)
#include "base/alloc.h"
#include "base/utf8.h"
#include "db/internal.h"
#include "db/turso.h"
#include "db/utils.h"

bool db_get_message_last_id(struct db *db, int64_t network_id, const char *name, int64_t *out,
			    struct err **err) {
	struct db_conn *dbc = db_network_conn(db, network_id, err);
	if (dbc == nullptr) {
		return false;
	}
	struct db_stmt *st = db_prepare(
		dbc,
		"SELECT id FROM Message\n"
		"                WHERE target = (SELECT id FROM MessageTarget WHERE network "
		"= :network AND target = :target)\n"
		"                AND text IS NOT NULL ORDER BY id DESC LIMIT 1",
		err);
	if (st == nullptr) {
		return false;
	}
	bind_i64(st, ":network", network_id);
	bind_text(st, ":target", name);
	int rc = db_step(st);
	*out = rc == SQLITE_ROW ? col_i64(st, 0) : 0;
	bool ok = rc == SQLITE_ROW || rc == SQLITE_DONE || db_sql_err(dbc, err);
	db_finalize(st);
	return ok;
}

bool db_get_message_target(struct db *db, int64_t network_id, const char *target,
			   struct db_message_target *out, struct err **err) {
	struct db_conn *dbc = db_network_conn(db, network_id, err);
	if (dbc == nullptr) {
		return false;
	}
	struct db_stmt *st =
		db_prepare(dbc,
			   "SELECT id, pinned, muted, blocked FROM MessageTarget WHERE "
			   "network = :network AND target = :target",
			   err);
	if (st == nullptr) {
		return false;
	}
	bind_i64(st, ":network", network_id);
	bind_text(st, ":target", target);
	int rc = db_step(st);
	struct db_message_target mt = {.target = xstrdup(target)};
	if (rc == SQLITE_ROW) {
		mt.id = col_i64(st, 0);
		mt.pinned = col_bool(st, 1);
		mt.muted = col_bool(st, 2);
		mt.blocked = col_bool(st, 3);
	}
	bool ok = rc == SQLITE_ROW || rc == SQLITE_DONE || db_sql_err(dbc, err);
	db_finalize(st);
	if (ok) {
		*out = mt;
	} else {
		db_message_target_free(&mt);
	}
	return ok;
}

bool db_list_message_targets(struct db *db, int64_t network_id, db_message_target_vec *out,
			     struct err **err) {
	struct db_conn *dbc = db_network_conn(db, network_id, err);
	if (dbc == nullptr) {
		return false;
	}
	struct db_stmt *st =
		db_prepare(dbc,
			   "SELECT id, target, pinned, muted, blocked\n"
			   "            FROM MessageTarget\n"
			   "            WHERE network = ? AND (pinned OR muted OR blocked)",
			   err);
	if (st == nullptr) {
		return false;
	}
	bind_pos_i64(st, 1, network_id);
	int rc;
	while ((rc = db_step(st)) == SQLITE_ROW) {
		vec_push(out, (struct db_message_target){
				      .id = col_i64(st, 0),
				      .target = col_lossy(st, 1),
				      .pinned = col_bool(st, 2),
				      .muted = col_bool(st, 3),
				      .blocked = col_bool(st, 4),
			      });
	}
	bool ok = rc == SQLITE_DONE || db_sql_err(dbc, err);
	db_finalize(st);
	return ok;
}

bool db_store_message_target(struct db *db, int64_t network_id, struct db_message_target *t,
			     struct err **err) {
	struct db_conn *dbc = db_network_conn(db, network_id, err);
	if (dbc == nullptr) {
		return false;
	}
	struct db_stmt *st;
	if (t->id != 0) {
		st = db_prepare(
			dbc,
			"UPDATE MessageTarget SET pinned = :pinned, muted = :muted, blocked = "
			":blocked WHERE id = :id AND (pinned IS NOT :pinned OR muted IS NOT "
			":muted OR blocked IS NOT :blocked)",
			err);
		if (st == nullptr) {
			return false;
		}
		bind_i64(st, ":id", t->id);
	} else {
		st = db_prepare(
			dbc,
			"INSERT INTO\n"
			"                MessageTarget(network, target, pinned, muted, blocked)\n"
			"                VALUES (:network, :target, :pinned, :muted, :blocked)",
			err);
		if (st == nullptr) {
			return false;
		}
		bind_i64(st, ":network", network_id);
		bind_text(st, ":target", t->target);
	}
	bind_i64(st, ":pinned", t->pinned);
	bind_i64(st, ":muted", t->muted);
	bind_i64(st, ":blocked", t->blocked);
	bool ok = db_step_done(st, err);
	db_finalize(st);
	if (ok && t->id == 0) {
		t->id = db_last_insert_rowid(dbc);
	}
	return ok;
}

// The stored time and searchable text of a message (text: nullptr for
// none)
bool message_time_text(const struct irc_msg *msg, char **time, char **text,
			      struct err **err) {
	struct timestamp t;
	const char *tag = irc_tags_get(&msg->tags, "time");
	if (tag != nullptr) {
		if (!parse_server_time(str_from(tag), &t)) {
			AUTOFREE char *q = utf8_debug_cstr(str_from(tag));
			return err_set(err, 0, "failed to parse message time tag: %s", q);
		}
	} else {
		t = timestamp_now();
	}
	*text = nullptr;
	if ((cstr_eq(msg->command, "PRIVMSG") || cstr_eq(msg->command, "NOTICE")) &&
	    msg->params.len > 1) {
		struct strbuf sb = STRBUF_INIT;
		strip_ansi(&sb, str_from(msg->params.data[1]));
		*text = strbuf_take(&sb);
	}
	*time = format_server_time_cstr(t);
	return true;
}

bool db_store_messages(struct db *db, int64_t network_id, const char *name,
		       struct irc_msg *const *msgs, size_t n, i64vec *ids, struct err **err) {
	struct db_conn *dbc = db_network_conn(db, network_id, err);
	if (dbc == nullptr) {
		return false;
	}
	if (n == 0) {
		return true;
	}
	if (dbc->insert_target == nullptr) {
		dbc->insert_target = db_prepare(dbc,
						"INSERT INTO MessageTarget(network, target)\n"
						"            VALUES (:network, :target)\n"
						"            ON CONFLICT DO NOTHING",
						err);
		if (dbc->insert_target == nullptr) {
			return false;
		}
	}
	if (dbc->insert_message == nullptr) {
		// Message IDs are delivery receipt cursors: never reuse the IDs of
		// deleted messages, see record_message_id_high_water
		// Turso tenants can't seek max(id) of their clustered table: IDs
		// are reserved like the background writer does
		dbc->insert_message = db_prepare(
			dbc,
			dbc->remote
				? "INSERT INTO Message(id, target, raw, time, sender, text)\n"
				  "                SELECT :id, id, :raw, :time, :sender, :text\n"
				  "                FROM MessageTarget WHERE network = :network AND "
				  "target = :target"
				: "INSERT INTO Message(id, target, raw, time, sender, text)\n"
				  "                SELECT max(\n"
				  "                        coalesce((SELECT max(id) FROM Message), "
				  "0),\n"
				  "                        coalesce((SELECT last_id FROM "
				  "MessageIdHighWater), 0)\n"
				  "                    ) + 1,\n"
				  "                    id, :raw, :time, :sender, :text\n"
				  "                FROM MessageTarget as t\n"
				  "                WHERE network = :network AND target = :target",
			err);
		if (dbc->insert_message == nullptr) {
			return false;
		}
	}
	if (!db_begin(dbc, err)) {
		return false;
	}
	struct db_stmt *st = dbc->insert_target;
	bind_i64(st, ":network", network_id);
	bind_text(st, ":target", name);
	bool ok = db_step_done(st, err);
	db_clear_bindings(st);
	size_t start = ids->len;
	int64_t first_id = 0;
	if (ok && dbc->remote) {
		struct db_stmt *reserve = db_prepare(
			dbc,
			"INSERT INTO MessageIdHighWater(id, last_id)\n"
			"                VALUES (0, coalesce((SELECT last_id FROM "
			"MessageIdHighWater "
			"WHERE id = 0), 0) + :count)\n"
			"                ON CONFLICT(id) DO UPDATE SET last_id = excluded.last_id\n"
			"                RETURNING last_id",
			err);
		ok = reserve != nullptr;
		if (ok) {
			bind_i64(reserve, ":count", (int64_t)n);
			ok = db_step(reserve) == SQLITE_ROW || db_sql_err(dbc, err);
			first_id = ok ? col_i64(reserve, 0) - (int64_t)n + 1 : 0;
			db_finalize(reserve);
		}
	}
	st = dbc->insert_message;
	for (size_t i = 0; ok && i < n; i++) {
		char *time = nullptr, *text = nullptr;
		if (!message_time_text(msgs[i], &time, &text, err)) {
			ok = false;
			break;
		}
		AUTOFREE char *raw = irc_msg_to_string(msgs[i]);
		if (dbc->remote) {
			bind_i64(st, ":id", first_id + (int64_t)i);
		}
		bind_i64(st, ":network", network_id);
		bind_text(st, ":target", name);
		bind_text(st, ":raw", raw);
		bind_text(st, ":time", time);
		bind_text(st, ":sender", irc_msg_prefix_name(msgs[i]));
		if (text != nullptr) {
			bind_text(st, ":text", text);
		} else {
			bind_null(st, ":text");
		}
		ok = db_step_done(st, err);
		db_clear_bindings(st);
		xfree(time);
		xfree(text);
		if (ok) {
			vec_push(ids,
				 dbc->remote ? first_id + (int64_t)i : db_last_insert_rowid(dbc));
		}
	}
	if (!ok || !db_commit(dbc, err)) {
		db_rollback(dbc);
		ids->len = start;
		return false;
	}
	return true;
}

bool db_list_message_last_per_target(struct db *db, int64_t network_id,
				     const struct db_message_options *o, db_target_last_vec *out,
				     struct err **err) {
	struct db_conn *dbc = db_network_conn(db, network_id, err);
	if (dbc == nullptr) {
		return false;
	}
	const char *inner =
		o->events && dbc->remote
			// Turso tenants cluster messages by target and time
			? "SELECT time FROM Message WHERE target=MessageTarget.id ORDER BY time "
			  "DESC "
			  "LIMIT 1"
			: o->events ? "SELECT max(latest) FROM (\n"
				      "                SELECT max(time) latest FROM Message WHERE "
				      "target=MessageTarget.id AND text IS NULL\n"
				      "                UNION ALL\n"
				      "                SELECT max(time) latest FROM Message WHERE "
				      "target=MessageTarget.id AND text IS NOT NULL\n"
				      "            )"
				    : "SELECT time FROM Message WHERE target=MessageTarget.id AND "
				      "text IS NOT "
				      "NULL ORDER BY time DESC LIMIT 1";
	AUTO_STRBUF struct strbuf q = STRBUF_INIT;
	strbuf_printf(&q,
		      "SELECT target, (%s) latest\n"
		      "            FROM MessageTarget\n"
		      "            WHERE network = :network ",
		      inner);
	if (o->after_time.set) {
		// compares time strings by lexicographical order
		strbuf_append_cstr(&q, "AND latest > :after ");
	}
	if (o->before_time.set) {
		strbuf_append_cstr(&q, "AND latest < :before ");
	}
	strbuf_append_cstr(&q, o->take_last ? "ORDER BY latest DESC " : "ORDER BY latest ASC ");
	strbuf_append_cstr(&q, "LIMIT :limit");
	struct db_stmt *st = db_prepare(dbc, strbuf_cstr(&q), err);
	if (st == nullptr) {
		return false;
	}
	bind_i64(st, ":network", network_id);
	if (o->after_time.set) {
		bind_time(st, ":after", &o->after_time);
	}
	if (o->before_time.set) {
		bind_time(st, ":before", &o->before_time);
	}
	bind_i64(st, ":limit", o->limit > INT64_MAX ? INT64_MAX : (int64_t)o->limit);
	db_target_last_vec v = {0};
	int rc;
	bool ok = true;
	while (ok && (rc = db_step(st)) == SQLITE_ROW) {
		struct db_target_last t = {.name = col_lossy(st, 0)};
		ok = col_time(st, 1, &t.latest_message, err);
		if (ok) {
			vec_push(&v, t);
		} else {
			xfree(t.name);
		}
	}
	if (ok && rc != SQLITE_DONE) {
		ok = db_sql_err(dbc, err);
	}
	db_finalize(st);
	if (!ok) {
		db_target_last_vec_free(&v);
		return false;
	}
	// Ordered by DESC to limit to the last ones: back to ASC
	for (size_t i = 0; i < v.len; i++) {
		vec_push(out, v.data[o->take_last ? v.len - 1 - i : i]);
	}
	vec_free(&v);
	return true;
}

bool db_list_message_rows(struct db *db, int64_t network_id, const char *name,
			  const struct db_message_options *o, db_message_row_vec *out,
			  struct err **err) {
	struct db_conn *dbc = db_network_conn(db, network_id, err);
	if (dbc == nullptr) {
		return false;
	}
	const char *search = o->text != nullptr ? o->text : "";
	const char *sender = o->sender != nullptr ? o->sender : "";
	AUTO_STRBUF struct strbuf text = STRBUF_INIT;
	quote_fts_query(&text, str_from(search));
	if (search[0] != '\0' && text.len == 0) {
		// Nothing to search for, e.g. only punctuation: MATCH '' would be a
		// syntax error
		return true;
	}
	static const char target[] =
		"(SELECT id FROM MessageTarget WHERE network = :network AND target = :target)";
	AUTO_STRBUF struct strbuf q = STRBUF_INIT;
	strbuf_append_cstr(&q, "SELECT m.id AS id, m.time AS time, m.raw AS raw FROM ");
	if (search[0] != '\0') {
		// Seek each search result by ID, rather than scanning the target's
		// history for them
		strbuf_append_cstr(&q, "(SELECT rowid AS r FROM MessageFTS WHERE MessageFTS MATCH "
				       ":text) AS f\n"
				       "                CROSS JOIN Message AS m ON m.id = f.r ");
	} else {
		strbuf_append_cstr(&q, "Message AS m ");
	}
	strbuf_printf(&q, "WHERE m.target = %s ", target);
	bool text_only = !o->events || search[0] != '\0';
	if (o->after_id > 0) {
		if (text_only) {
			// Delivery cursors: seek the text message ID index
			strbuf_printf(
				&q,
				"AND (m.time, m.id) IN (SELECT time, id FROM Message\n"
				"                    WHERE target = %s AND text IS NOT NULL AND "
				"id > :afterID) ",
				target);
		} else {
			strbuf_append_cstr(&q, "AND m.id > :afterID ");
		}
	}
	if (o->after_time.set) {
		strbuf_append_cstr(&q, "AND m.time > :after ");
	}
	if (o->before_time.set) {
		strbuf_append_cstr(&q, "AND m.time < :before ");
	}
	if (sender[0] != '\0') {
		strbuf_append_cstr(&q, "AND m.sender = :sender ");
	}
	if (text_only) {
		strbuf_append_cstr(&q, "AND m.text IS NOT NULL ");
	} else if (!dbc->remote) {
		// Merge two disjoint ordered indexes, avoiding a second timestamp
		// index entry on every text message. LIMIT applies to their union.
		AUTOFREE char *text_query = xasprintf("%sAND m.text IS NOT NULL ", strbuf_cstr(&q));
		strbuf_append_cstr(&q, "AND m.text IS NULL UNION ALL ");
		strbuf_append_cstr(&q, text_query);
	}
	// Messages often share a time, e.g. the lines of a multiline message:
	// order them as they were stored
	strbuf_append_cstr(&q, o->take_last ? "ORDER BY time DESC, id DESC "
					    : "ORDER BY time ASC, id ASC ");
	strbuf_append_cstr(&q, "LIMIT :limit");

	struct db_stmt *st = db_prepare(dbc, strbuf_cstr(&q), err);
	if (st == nullptr) {
		return false;
	}
	bind_i64(st, ":network", network_id);
	bind_text(st, ":target", name);
	if (search[0] != '\0') {
		bind_text(st, ":text", strbuf_cstr(&text));
	}
	if (o->after_id > 0) {
		bind_i64(st, ":afterID", o->after_id);
	}
	if (o->after_time.set) {
		bind_time(st, ":after", &o->after_time);
	}
	if (o->before_time.set) {
		bind_time(st, ":before", &o->before_time);
	}
	if (sender[0] != '\0') {
		bind_text(st, ":sender", sender);
	}
	bind_i64(st, ":limit", o->limit > INT64_MAX ? INT64_MAX : (int64_t)o->limit);

	db_message_row_vec v = {0};
	int rc;
	bool ok = true;
	while (ok && (rc = db_step(st)) == SQLITE_ROW) {
		AUTOFREE char *raw = col_lossy(st, 2);
		enum irc_parse_error pe;
		struct irc_msg *m = irc_msg_parse(str_from(raw), &pe);
		if (m == nullptr) {
			ok = err_set(err, 0, "%s", irc_parse_error_str(pe));
			break;
		}
		vec_push(&v, (struct db_message_row){
				     .id = col_i64(st, 0),
				     .time = col_lossy(st, 1),
				     .message = m,
			     });
	}
	if (ok && rc != SQLITE_DONE) {
		ok = db_sql_err(dbc, err);
	}
	db_finalize(st);
	if (!ok) {
		db_message_row_vec_free(&v);
		return false;
	}
	for (size_t i = 0; i < v.len; i++) {
		vec_push(out, v.data[o->take_last ? v.len - 1 - i : i]);
	}
	vec_free(&v);
	return true;
}

bool db_list_messages(struct db *db, int64_t network_id, const char *name,
		      const struct db_message_options *o, irc_msg_vec *out, struct err **err) {
	struct db_conn *dbc = db_network_conn(db, network_id, err);
	if (dbc == nullptr) {
		return false;
	}
	db_message_row_vec rows = {0};
	if (!db_list_message_rows(db, network_id, name, o, &rows, err)) {
		return false;
	}
	for (size_t i = 0; i < rows.len; i++) {
		vec_push(out, rows.data[i].message);
		xfree(rows.data[i].time);
	}
	vec_free(&rows);
	return true;
}

bool record_message_id_high_water(struct db_conn *dbc, struct err **err) {
	// Turso tenants reserve every ID through this row before inserting:
	// their clustered table can't seek max(id)
	if (dbc->remote) {
		return true;
	}
	return db_exec_one(dbc,
			   "INSERT INTO MessageIdHighWater(id, last_id)\n"
			   "        SELECT 0,id FROM Message WHERE true ORDER BY id DESC LIMIT 1\n"
			   "        ON CONFLICT(id) DO UPDATE SET last_id=excluded.last_id\n"
			   "        WHERE last_id<excluded.last_id",
			   err);
}

// One database's part of db_delete_messages_before
static bool delete_before(struct db_conn *dbc, struct timestamp before, size_t limit,
			  size_t *deleted, struct err **err) {
	*deleted = 0;
	if (limit == 0) {
		return true;
	}
	if (!db_begin(dbc, err)) {
		return false;
	}
	bool ok = record_message_id_high_water(dbc, err);
	struct db_stmt *st = nullptr;
	if (ok) {
		st = db_prepare(
			dbc,
			// Seek each target's oldest messages in the clustered table
			dbc->remote
				? "DELETE FROM Message WHERE (target, time, id) IN (\n"
				  "                    SELECT m.target, m.time, m.id FROM "
				  "MessageTarget t\n"
				  "                    CROSS JOIN Message m ON m.target = t.id\n"
				  "                    WHERE m.time < :before LIMIT :limit\n"
				  "                )"
				: "DELETE FROM Message WHERE id IN (\n"
				  "                SELECT id FROM (\n"
				  "                    SELECT m.id FROM MessageTarget t CROSS JOIN "
				  "Message m "
				  "ON m.target=t.id\n"
				  "                    WHERE m.text IS NULL AND m.time<:before\n"
				  "                    UNION ALL\n"
				  "                    SELECT m.id FROM MessageTarget t CROSS JOIN "
				  "Message m "
				  "ON m.target=t.id\n"
				  "                    WHERE m.text IS NOT NULL AND "
				  "m.time<:before\n"
				  "                ) LIMIT :limit\n"
				  "            )",
			err);
		ok = st != nullptr;
	}
	if (ok) {
		AUTOFREE char *b = format_server_time_cstr(before);
		bind_text(st, ":before", b);
		bind_i64(st, ":limit", limit > INT64_MAX ? INT64_MAX : (int64_t)limit);
		ok = db_step_done(st, err);
		*deleted = (size_t)db_changes(dbc);
	}
	db_finalize(st);
	if (!ok || !db_commit(dbc, err)) {
		db_rollback(dbc);
		*deleted = 0;
		return false;
	}
	return true;
}

bool db_delete_messages_before(struct db *db, struct timestamp before, size_t limit,
			       size_t *deleted, struct err **err) {
	if (db->cloud == nullptr) {
		return delete_before(db->account, before, limit, deleted, err);
	}
	*deleted = 0;
	for (size_t i = 0; i < db->cloud->users.len && *deleted < limit; i++) {
		size_t n = 0;
		if (!delete_before(&db->cloud->users.data[i]->conn, before, limit - *deleted, &n,
				   err)) {
			return false;
		}
		*deleted += n;
	}
	return true;
}

// A token character for FTS5's unicode61 tokenizer: general categories L*,
// N* and Co
static bool is_fts_token_char(uint32_t c) {
	return utf8_is_alphanumeric(c) || (c >= 0xE000 && c <= 0xF8FF) ||
	       (c >= 0xF0000 && c <= 0xFFFFD) || (c >= 0x100000 && c <= 0x10FFFD);
}

void quote_fts_query(struct strbuf *out, struct str query) {
	size_t i = 0, start = 0;
	bool first = true;
	uint32_t c;
	while (true) {
		size_t before = i;
		bool more = utf8_next(query, &i, &c);
		if (!more || !is_fts_token_char(c)) {
			if (before > start) {
				if (!first) {
					strbuf_append_char(out, ' ');
				}
				first = false;
				strbuf_append_char(out, '"');
				// tokens hold no '"' (not a token char)
				strbuf_append(out, query.p + start, before - start);
				strbuf_append_char(out, '"');
			}
			start = i;
		}
		if (!more) {
			break;
		}
	}
}
