// Delivery and read receipts (soju-rs database/sqlite/receipts.rs)
#include "base/alloc.h"
#include "base/map.h"
#include "db/internal.h"

static void bind_client(sqlite3_stmt *st, int i, const char *client) {
	if (client[0] == '\0') {
		sqlite3_bind_null(st, i);
	} else {
		sqlite3_bind_text(st, i, client, -1, SQLITE_TRANSIENT);
	}
}

bool db_list_delivery_receipts(struct db *db, int64_t network_id, db_receipt_vec *out,
			       struct err **err) {
	sqlite3_stmt *st = db_prepare(db,
				      "SELECT id, target, client, internal_msgid\n"
				      "            FROM DeliveryReceipt\n"
				      "            WHERE network = ?",
				      err);
	if (st == nullptr) {
		return false;
	}
	sqlite3_bind_int64(st, 1, network_id);
	int rc;
	while ((rc = sqlite3_step(st)) == SQLITE_ROW) {
		vec_push(out, (struct db_delivery_receipt){
				      .id = col_i64(st, 0),
				      .target = col_lossy(st, 1),
				      .client = col_lossy(st, 2),
				      .internal_msg_id = col_lossy(st, 3),
			      });
	}
	bool ok = rc == SQLITE_DONE || db_sql_err(db, err);
	sqlite3_finalize(st);
	return ok;
}

enum change_kind {
	CHANGE_DELETE,
	CHANGE_UPDATE,
	CHANGE_INSERT,
};

struct change {
	enum change_kind kind;
	int64_t id;
	size_t i;
};

struct saved {
	int64_t id;
	char *message;
};

static void saved_free(void *p) {
	struct saved *s = p;
	if (s != nullptr) {
		xfree(s->message);
		xfree(s);
	}
}

// Writes only what changed: deletes duplicates and receipts of targets no
// longer listed, updates changed message IDs, inserts new targets
static bool store_changes(struct db *db, int64_t network_id, const char *client,
			  struct db_delivery_receipt *receipts, size_t n, struct err **err) {
	sqlite3_stmt *st = db_prepare(db,
				      "SELECT id,target,internal_msgid FROM DeliveryReceipt WHERE "
				      "network=? AND client IS ? ORDER BY id",
				      err);
	if (st == nullptr) {
		return false;
	}
	sqlite3_bind_int64(st, 1, network_id);
	bind_client(st, 2, client);
	struct strmap existing;
	strmap_init(&existing, saved_free);
	VEC(struct change) changes = {0};
	int64_t *ids = xcalloc(n > 0 ? n : 1, sizeof(*ids));
	bool ok = true;
	int rc;
	while ((rc = sqlite3_step(st)) == SQLITE_ROW) {
		AUTOFREE char *target = col_lossy(st, 1);
		struct saved *s = xmalloc(sizeof(*s));
		*s = (struct saved){.id = col_i64(st, 0), .message = col_lossy(st, 2)};
		struct saved *previous = strmap_get(&existing, target);
		if (previous != nullptr) {
			vec_push(&changes, (struct change){CHANGE_DELETE, previous->id, 0});
		}
		strmap_set(&existing, target, s);
	}
	if (rc != SQLITE_DONE) {
		ok = db_sql_err(db, err);
	}
	sqlite3_finalize(st);
	for (size_t i = 0; ok && i < n; i++) {
		struct saved *s = strmap_take(&existing, receipts[i].target);
		if (s != nullptr) {
			ids[i] = s->id;
			if (!cstr_eq(s->message, receipts[i].internal_msg_id)) {
				vec_push(&changes, (struct change){CHANGE_UPDATE, s->id, i});
			}
			saved_free(s);
		} else {
			vec_push(&changes, (struct change){CHANGE_INSERT, 0, i});
		}
	}
	const char *k;
	void *v;
	// The Rust code takes the remaining ones from a HashMap: their order
	// doesn't matter, deletes are independent
	for (size_t it = 0; ok && strmap_next(&existing, &it, &k, &v);) {
		vec_push(&changes, (struct change){CHANGE_DELETE, ((struct saved *)v)->id, 0});
	}
	strmap_free(&existing);

	for (size_t c = 0; ok && c < changes.len; c++) {
		struct change ch = changes.data[c];
		struct db_delivery_receipt *r = &receipts[ch.i];
		switch (ch.kind) {
		case CHANGE_DELETE:
			ok = db_exec_i64(db, "DELETE FROM DeliveryReceipt WHERE id=?", ch.id, err);
			break;
		case CHANGE_UPDATE:
			st = db_prepare(
				db, "UPDATE DeliveryReceipt SET internal_msgid=? WHERE id=?", err);
			ok = st != nullptr;
			if (ok) {
				sqlite3_bind_text(st, 1, r->internal_msg_id, -1, SQLITE_TRANSIENT);
				sqlite3_bind_int64(st, 2, ch.id);
				ok = db_step_done(db, st, err);
				sqlite3_finalize(st);
			}
			break;
		case CHANGE_INSERT:
			st = db_prepare(db,
					"INSERT INTO "
					"DeliveryReceipt(id,network,target,client,internal_msgid) "
					"VALUES(?,?,?,?,?)",
					err);
			ok = st != nullptr;
			if (ok) {
				sqlite3_bind_null(st, 1);
				sqlite3_bind_int64(st, 2, network_id);
				sqlite3_bind_text(st, 3, r->target, -1, SQLITE_TRANSIENT);
				bind_client(st, 4, client);
				sqlite3_bind_text(st, 5, r->internal_msg_id, -1, SQLITE_TRANSIENT);
				ok = db_step_done(db, st, err);
				sqlite3_finalize(st);
				ids[ch.i] = sqlite3_last_insert_rowid(db->conn);
			}
			break;
		}
	}
	if (ok) {
		for (size_t i = 0; i < n; i++) {
			receipts[i].id = ids[i];
		}
	}
	vec_free(&changes);
	xfree(ids);
	return ok;
}

// Preserves the old NULL-client duplicate semantics for unusual callers
static bool replace_duplicates(struct db *db, int64_t network_id, const char *client,
			       struct db_delivery_receipt *receipts, size_t n, struct err **err) {
	sqlite3_stmt *st = db_prepare(
		db, "DELETE FROM DeliveryReceipt WHERE network = ? AND client IS ?", err);
	if (st == nullptr) {
		return false;
	}
	sqlite3_bind_int64(st, 1, network_id);
	bind_client(st, 2, client);
	bool ok = db_step_done(db, st, err);
	sqlite3_finalize(st);
	for (size_t i = 0; ok && i < n; i++) {
		st = db_prepare(
			db,
			"INSERT INTO DeliveryReceipt(network, target, client, "
			"internal_msgid)\n"
			"                VALUES (:network, :target, :client, :internal_msgid)",
			err);
		if (st == nullptr) {
			return false;
		}
		bind_i64(st, ":network", network_id);
		bind_text(st, ":target", receipts[i].target);
		bind_text_or_null(st, ":client", client);
		bind_text(st, ":internal_msgid", receipts[i].internal_msg_id);
		ok = db_step_done(db, st, err);
		sqlite3_finalize(st);
		receipts[i].id = sqlite3_last_insert_rowid(db->conn);
	}
	return ok;
}

bool db_store_client_delivery_receipts(struct db *db, int64_t network_id, const char *client,
				       struct db_delivery_receipt *receipts, size_t n,
				       struct err **err) {
	struct strmap unique;
	strmap_init(&unique, nullptr);
	for (size_t i = 0; i < n; i++) {
		strmap_set(&unique, receipts[i].target, nullptr);
	}
	bool dup = strmap_len(&unique) != n;
	strmap_free(&unique);
	if (!db_begin(db, err)) {
		return false;
	}
	bool ok = dup ? replace_duplicates(db, network_id, client, receipts, n, err)
		      : store_changes(db, network_id, client, receipts, n, err);
	if (!ok || !db_commit(db, err)) {
		db_rollback(db);
		return false;
	}
	return true;
}

bool db_get_read_receipt(struct db *db, int64_t network_id, const char *name,
			 struct db_read_receipt *out, bool *found, struct err **err) {
	*found = false;
	sqlite3_stmt *st =
		db_prepare(db,
			   "SELECT id, timestamp FROM ReadReceipt WHERE network = :network "
			   "AND target = :target",
			   err);
	if (st == nullptr) {
		return false;
	}
	bind_i64(st, ":network", network_id);
	bind_text(st, ":target", name);
	int rc = sqlite3_step(st);
	bool ok = true;
	if (rc == SQLITE_ROW) {
		struct db_read_receipt r = {.id = col_i64(st, 0)};
		ok = col_time(st, 1, &r.timestamp, err);
		if (ok) {
			r.target = xstrdup(name);
			*out = r;
			*found = true;
		}
	} else if (rc != SQLITE_DONE) {
		ok = db_sql_err(db, err);
	}
	sqlite3_finalize(st);
	return ok;
}

bool db_store_read_receipt(struct db *db, int64_t network_id, struct db_read_receipt *r,
			   struct err **err) {
	sqlite3_stmt *st;
	if (r->id != 0) {
		st = db_prepare(db,
				"UPDATE ReadReceipt SET timestamp = :timestamp WHERE id = :id AND "
				"(timestamp IS NOT :timestamp)",
				err);
		if (st == nullptr) {
			return false;
		}
		bind_i64(st, ":id", r->id);
	} else {
		st = db_prepare(db,
				"INSERT INTO ReadReceipt(network, target, timestamp)\n"
				"                VALUES (:network, :target, :timestamp)",
				err);
		if (st == nullptr) {
			return false;
		}
		bind_i64(st, ":network", network_id);
		bind_text(st, ":target", r->target);
	}
	bind_time(st, ":timestamp", &r->timestamp);
	bool ok = db_step_done(db, st, err);
	sqlite3_finalize(st);
	if (ok && r->id == 0) {
		r->id = sqlite3_last_insert_rowid(db->conn);
	}
	return ok;
}
