// Web Push configuration and subscriptions (soju-rs
// database/sqlite/webpush.rs)
#include "base/alloc.h"
#include "db/internal.h"
#include "db/turso.h"

bool db_list_webpush_configs(struct db *db, db_webpush_config_vec *out, struct err **err) {
	struct db_conn *dbc = db->account;
	struct db_stmt *st = db_prepare(
		dbc, "SELECT id, vapid_key_public, vapid_key_private FROM WebPushConfig", err);
	if (st == nullptr) {
		return false;
	}
	int rc;
	bool ok = true;
	while (ok && (rc = db_step(st)) == SQLITE_ROW) {
		struct db_webpush_config c = {.id = col_i64(st, 0)};
		ok = col_text(st, 1, &c.vapid_public, err) &&
		     col_text(st, 2, &c.vapid_private, err);
		if (ok) {
			vec_push(out, c);
		} else {
			db_webpush_config_free(&c);
		}
	}
	if (ok && rc != SQLITE_DONE) {
		ok = db_sql_err(dbc, err);
	}
	db_finalize(st);
	return ok;
}

bool db_store_webpush_config(struct db *db, struct db_webpush_config *c, struct err **err) {
	struct db_conn *dbc = db->account;
	if (c->id != 0) {
		return err_set(err, 0, "cannot update a WebPushConfig");
	}
	struct db_stmt *st = db_prepare(
		dbc,
		"INSERT INTO WebPushConfig(created_at, vapid_key_public, vapid_key_private)\n"
		"            VALUES (:now, :vapid_key_public, :vapid_key_private)",
		err);
	if (st == nullptr) {
		return false;
	}
	bind_text(st, ":vapid_key_public", c->vapid_public);
	bind_text(st, ":vapid_key_private", c->vapid_private);
	AUTOFREE char *now = format_server_time_cstr(timestamp_now());
	bind_text(st, ":now", now);
	bool ok = db_step_done(st, err);
	db_finalize(st);
	if (ok) {
		c->id = db_last_insert_rowid(dbc);
	}
	return ok;
}

bool db_list_webpush_networks(struct db *db, int64_t user_id, i64vec *out, struct err **err) {
	struct db_conn *dbc = db_user_conn(db, user_id, err);
	if (dbc == nullptr) {
		return false;
	}
	struct db_stmt *st = db_prepare(
		dbc, "SELECT DISTINCT coalesce(network, 0) FROM WebPushSubscription WHERE user = ?",
		err);
	if (st == nullptr) {
		return false;
	}
	bind_pos_i64(st, 1, user_id);
	int rc;
	while ((rc = db_step(st)) == SQLITE_ROW) {
		vec_push(out, col_i64(st, 0));
	}
	bool ok = rc == SQLITE_DONE || db_sql_err(dbc, err);
	db_finalize(st);
	return ok;
}

bool db_list_webpush_subscriptions(struct db *db, int64_t user_id, int64_t network_id,
				   db_webpush_sub_vec *out, struct err **err) {
	struct db_conn *dbc = db_user_conn(db, user_id, err);
	if (dbc == nullptr) {
		return false;
	}
	size_t start = out->len;
	struct db_stmt *st =
		db_prepare(dbc,
			   "SELECT id, endpoint, created_at, updated_at, key_auth, key_p256dh, "
			   "key_vapid\n"
			   "            FROM WebPushSubscription\n"
			   "            WHERE user = ? AND network IS ?",
			   err);
	if (st == nullptr) {
		return false;
	}
	bind_pos_i64(st, 1, user_id);
	if (network_id != 0) {
		bind_pos_i64(st, 2, network_id);
	} else {
		bind_pos_null(st, 2);
	}
	int rc;
	bool ok = true;
	while (ok && (rc = db_step(st)) == SQLITE_ROW) {
		struct db_webpush_subscription s = {.id = col_i64(st, 0)};
		ok = col_text(st, 1, &s.endpoint, err) && col_time(st, 2, &s.created_at, err) &&
		     col_time(st, 3, &s.updated_at, err) && col_text_opt(st, 4, &s.key_auth, err) &&
		     col_text_opt(st, 5, &s.key_p256dh, err) &&
		     col_text_opt(st, 6, &s.key_vapid, err);
		if (ok) {
			vec_push(out, s);
		} else {
			db_webpush_subscription_free(&s);
		}
	}
	if (ok && rc != SQLITE_DONE) {
		ok = db_sql_err(dbc, err);
	}
	db_finalize(st);
	// Deleting a subscription by ID, e.g. when a push service reports it
	// expired, would otherwise search every Turso user database for it
	for (size_t k = start; ok && db->cloud != nullptr && k < out->len; k++) {
		ok = turso_remember(db->cloud, "WebPushSubscription", user_id, out->data[k].id,
				    err);
	}
	return ok;
}

bool db_store_webpush_subscription(struct db *db, int64_t user_id, int64_t network_id,
				   struct db_webpush_subscription *s, struct err **err) {
	struct db_conn *dbc = db_user_conn(db, user_id, err);
	if (dbc == nullptr) {
		return false;
	}
	AUTOFREE char *now = format_server_time_cstr(timestamp_now());
	struct db_stmt *st;
	if (s->id != 0) {
		st = db_prepare(
			dbc,
			"UPDATE WebPushSubscription\n"
			"                SET updated_at = :now, key_auth = :key_auth, "
			"key_p256dh = :key_p256dh,\n"
			"                    key_vapid = :key_vapid\n"
			"                WHERE id = :id AND (updated_at IS NOT :now OR key_auth "
			"IS NOT :key_auth OR key_p256dh IS NOT :key_p256dh OR key_vapid IS NOT "
			":key_vapid)",
			err);
		if (st == nullptr) {
			return false;
		}
		bind_i64(st, ":id", s->id);
	} else {
		st = db_prepare(
			dbc,
			"INSERT INTO\n"
			"                WebPushSubscription(id, created_at, updated_at, user, "
			"network, endpoint,\n"
			"                    key_auth, key_p256dh, key_vapid)\n"
			"                VALUES (:id, :now, :now, :user, :network, :endpoint, "
			":key_auth,\n"
			"                    :key_p256dh, :key_vapid)",
			err);
		if (st == nullptr) {
			return false;
		}
		int64_t id = db_new_row_id(dbc);
		if (id != 0) {
			bind_i64(st, ":id", id);
		} else {
			bind_null(st, ":id");
		}
		bind_i64(st, ":user", user_id);
		if (network_id != 0) {
			bind_i64(st, ":network", network_id);
		} else {
			bind_null(st, ":network");
		}
		bind_text(st, ":endpoint", s->endpoint);
	}
	bind_text(st, ":now", now);
	bind_text(st, ":key_auth", s->key_auth);
	bind_text(st, ":key_p256dh", s->key_p256dh);
	bind_text(st, ":key_vapid", s->key_vapid);
	bool ok = db_step_done(st, err);
	db_finalize(st);
	if (ok && s->id == 0) {
		s->id = db_last_insert_rowid(dbc);
	}
	return ok && (db->cloud == nullptr ||
		      turso_remember(db->cloud, "WebPushSubscription", user_id, s->id, err));
}

bool db_delete_webpush_subscription(struct db *db, int64_t id, struct err **err) {
	struct db_conn *dbc = db_subscription_conn(db, id, err);
	if (dbc == nullptr) {
		return false;
	}
	return db_exec_i64(dbc, "DELETE FROM WebPushSubscription WHERE id = ?", id, err);
}
