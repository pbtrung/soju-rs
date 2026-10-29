// Users and their device certificates (soju-rs database/sqlite/users.rs)
#include "base/alloc.h"
#include "db/internal.h"
#include "db/turso.h"

// Columns: id, [username], password, admin, nick, realname, enabled,
// downstream_interacted_at, max_networks
static bool user_from_row(struct db_stmt *st, const char *username, struct db_user *u,
			  struct err **err) {
	int off = username != nullptr ? 0 : 1;
	*u = (struct db_user){.id = col_i64(st, 0)};
	bool ok = true;
	if (username != nullptr) {
		u->username = xstrdup(username);
	} else {
		ok = col_text(st, 1, &u->username, err);
	}
	ok = ok && col_text_opt(st, 1 + off, &u->password, err);
	u->admin = col_bool(st, 2 + off);
	u->nick = col_lossy(st, 3 + off);
	u->realname = col_lossy(st, 4 + off);
	u->enabled = col_bool(st, 5 + off);
	u->max_networks = col_i64(st, 7 + off);
	ok = ok && col_time(st, 6 + off, &u->downstream_interacted_at, err);
	if (u->username == nullptr) {
		u->username = xstrdup("");
	}
	if (u->password == nullptr) {
		u->password = xstrdup("");
	}
	if (!ok) {
		db_user_free(u);
	}
	return ok;
}

bool db_list_users(struct db *db, db_user_vec *out, struct err **err) {
	struct db_conn *dbc = db->account;
	struct db_stmt *st =
		db_prepare(dbc,
			   "SELECT id, username, password, admin, nick, realname, enabled,\n"
			   "                downstream_interacted_at, max_networks\n"
			   "            FROM User",
			   err);
	if (st == nullptr) {
		return false;
	}
	int rc;
	db_user_vec v = {0};
	while ((rc = db_step(st)) == SQLITE_ROW) {
		struct db_user u;
		if (!user_from_row(st, nullptr, &u, err)) {
			goto fail;
		}
		vec_push(&v, u);
	}
	if (rc != SQLITE_DONE) {
		db_sql_err(dbc, err);
		goto fail;
	}
	db_finalize(st);
	for (size_t i = 0; i < v.len; i++) {
		// Turso users being provisioned or deleted aren't loaded
		bool ready = true;
		if (db->cloud != nullptr && !turso_ready(db->cloud, v.data[i].id, &ready, err)) {
			for (size_t j = i; j < v.len; j++) {
				db_user_free(&v.data[j]);
			}
			vec_free(&v);
			return false;
		}
		if (ready) {
			vec_push(out, v.data[i]);
		} else {
			db_user_free(&v.data[i]);
		}
	}
	vec_free(&v);
	return true;
fail:
	db_finalize(st);
	db_user_vec_free(&v);
	return false;
}

bool db_get_user(struct db *db, const char *username, struct db_user *out, bool *found,
		 struct err **err) {
	struct db_conn *dbc = db->account;
	*found = false;
	struct db_stmt *st =
		db_prepare(dbc,
			   "SELECT id, password, admin, nick, realname, enabled,\n"
			   "                    downstream_interacted_at, max_networks\n"
			   "                FROM User\n"
			   "                WHERE username = ?",
			   err);
	if (st == nullptr) {
		return false;
	}
	bind_pos_text(st, 1, username);
	int rc = db_step(st);
	bool ok = true;
	if (rc == SQLITE_ROW) {
		ok = user_from_row(st, username, out, err);
		*found = ok;
	} else if (rc != SQLITE_DONE) {
		ok = db_sql_err(dbc, err);
	}
	db_finalize(st);
	bool ready = true;
	if (ok && *found && db->cloud != nullptr &&
	    (!(ok = turso_ready(db->cloud, out->id, &ready, err)) || !ready)) {
		db_user_free(out);
		*found = false;
	}
	return ok;
}

bool db_get_username_by_id(struct db *db, int64_t id, char **out, struct err **err) {
	struct db_conn *dbc = db->account;
	*out = nullptr;
	struct db_stmt *st = db_prepare(dbc, "SELECT username FROM User WHERE id = ?", err);
	if (st == nullptr) {
		return false;
	}
	bind_pos_i64(st, 1, id);
	int rc = db_step(st);
	bool ok = true;
	if (rc == SQLITE_ROW) {
		ok = col_text(st, 0, out, err);
	} else if (rc != SQLITE_DONE) {
		ok = db_sql_err(dbc, err);
	}
	db_finalize(st);
	return ok;
}

bool db_list_inactive_usernames(struct db *db, struct timestamp limit, strvec *out,
				struct err **err) {
	struct db_conn *dbc = db->account;
	struct db_stmt *st = db_prepare(dbc,
					"SELECT username FROM User\n"
					"            WHERE coalesce(downstream_interacted_at, "
					"created_at) < ? AND enabled = 1",
					err);
	if (st == nullptr) {
		return false;
	}
	AUTOFREE char *t = format_server_time_cstr(limit);
	bind_pos_text(st, 1, t);
	int rc;
	strvec v = {0};
	bool ok = true;
	while (ok && (rc = db_step(st)) == SQLITE_ROW) {
		char *name;
		ok = col_text(st, 0, &name, err);
		if (ok) {
			strvec_push_take(&v, name);
		}
	}
	if (ok && rc != SQLITE_DONE) {
		ok = db_sql_err(dbc, err);
	}
	db_finalize(st);
	if (ok) {
		for (size_t i = 0; i < v.len; i++) {
			strvec_push_take(out, v.data[i]);
		}
		vec_free(&v);
	} else {
		strvec_free(&v);
	}
	return ok;
}

static void bind_user_fields(struct db_stmt *st, const struct db_user *u, bool password) {
	bind_text(st, ":username", u->username);
	if (password) {
		bind_text_or_null(st, ":password", u->password);
	}
	bind_i64(st, ":admin", u->admin);
	bind_text_or_null(st, ":nick", u->nick);
	bind_text_or_null(st, ":realname", u->realname);
	bind_i64(st, ":enabled", u->enabled);
	bind_time(st, ":downstream_interacted_at", &u->downstream_interacted_at);
	bind_i64(st, ":max_networks", u->max_networks);
}

static bool run(struct db_stmt *st, struct err **err) {
	bool ok = db_step_done(st, err);
	db_finalize(st);
	return ok;
}

bool db_store_user_keep_password(struct db *db, const struct db_user *u, struct err **err) {
	struct db_conn *dbc = db->account;
	struct db_stmt *st =
		db_prepare(dbc,
			   "UPDATE User\n"
			   "            SET admin = :admin, nick = :nick, realname = :realname,\n"
			   "                enabled = :enabled, downstream_interacted_at = "
			   ":downstream_interacted_at,\n"
			   "                max_networks = :max_networks\n"
			   "            WHERE username = :username AND (admin IS NOT :admin OR "
			   "nick IS NOT :nick "
			   "OR realname IS NOT :realname OR enabled IS NOT :enabled OR "
			   "downstream_interacted_at "
			   "IS NOT :downstream_interacted_at OR max_networks IS NOT :max_networks)",
			   err);
	if (st == nullptr) {
		return false;
	}
	bind_user_fields(st, u, false);
	return run(st, err);
}

bool db_upgrade_user_password(struct db *db, int64_t id, const char *old, const char *new_hash,
			      struct err **err) {
	struct db_conn *dbc = db->account;
	struct db_stmt *st =
		db_prepare(dbc,
			   "UPDATE User SET password = :new WHERE id = :id AND password = "
			   ":old AND (password IS NOT :new)",
			   err);
	if (st == nullptr) {
		return false;
	}
	bind_i64(st, ":id", id);
	bind_text(st, ":old", old);
	bind_text(st, ":new", new_hash);
	return run(st, err);
}

bool db_set_user_downstream_interacted_at(struct db *db, int64_t id, struct timestamp t,
					  struct err **err) {
	struct db_conn *dbc = db->account;
	struct db_stmt *st =
		db_prepare(dbc,
			   "UPDATE User SET downstream_interacted_at = ?1 WHERE id = ?2 AND "
			   "downstream_interacted_at IS NOT ?1",
			   err);
	if (st == nullptr) {
		return false;
	}
	AUTOFREE char *s = format_server_time_cstr(t);
	bind_pos_text(st, 1, s);
	bind_pos_i64(st, 2, id);
	return run(st, err);
}

bool db_store_user(struct db *db, struct db_user *u, struct err **err) {
	if (db->cloud != nullptr && u->id == 0) {
		return turso_create_user(db->cloud, u, err);
	}
	struct db_conn *dbc = db->account;
	struct db_stmt *st;
	if (u->id != 0) {
		st = db_prepare(
			dbc,
			"UPDATE User\n"
			"                SET password = :password, admin = :admin, nick = :nick,\n"
			"                    realname = :realname, enabled = :enabled,\n"
			"                    downstream_interacted_at = "
			":downstream_interacted_at,\n"
			"                    max_networks = :max_networks\n"
			"                WHERE username = :username AND (password IS NOT :password "
			"OR "
			"admin IS NOT :admin OR nick IS NOT :nick OR realname IS NOT :realname OR "
			"enabled IS NOT :enabled OR downstream_interacted_at IS NOT "
			":downstream_interacted_at OR max_networks IS NOT :max_networks)",
			err);
		if (st == nullptr) {
			return false;
		}
		bind_user_fields(st, u, true);
		return run(st, err);
	}
	st = db_prepare(
		dbc,
		"INSERT INTO\n"
		"                User(id, username, password, admin, nick, realname, created_at,\n"
		"                    enabled, downstream_interacted_at, max_networks)\n"
		"                VALUES ((SELECT max(coalesce((SELECT max(id) FROM "
		"User),0),coalesce((SELECT last_id FROM ActorIdHighWater WHERE "
		"kind='user'),0))+1), :username, :password, :admin, :nick, :realname, :now,\n"
		"                    :enabled, :downstream_interacted_at, :max_networks)",
		err);
	if (st == nullptr) {
		return false;
	}
	bind_user_fields(st, u, true);
	AUTOFREE char *now = format_server_time_cstr(timestamp_now());
	bind_text(st, ":now", now);
	if (!run(st, err)) {
		return false;
	}
	u->id = db_last_insert_rowid(dbc);
	return true;
}

bool db_delete_user(struct db *db, int64_t id, struct err **err) {
	if (db->cloud != nullptr) {
		return turso_delete_user(db->cloud, id, err);
	}
	struct db_conn *dbc = db->account;
	static const char *const deletes[] = {
		("DELETE FROM DeliveryReceipt\n"
		 "            WHERE id IN (\n"
		 "                SELECT DeliveryReceipt.id\n"
		 "                FROM DeliveryReceipt\n"
		 "                JOIN Network ON DeliveryReceipt.network = Network.id\n"
		 "                WHERE Network.user = ?\n"
		 "            )"),
		("DELETE FROM ReadReceipt\n"
		 "            WHERE id IN (\n"
		 "                SELECT ReadReceipt.id\n"
		 "                FROM ReadReceipt\n"
		 "                JOIN Network ON ReadReceipt.network = Network.id\n"
		 "                WHERE Network.user = ?\n"
		 "            )"),
		nullptr, // record_message_id_high_water
		("DELETE FROM Message\n"
		 "            WHERE id IN (\n"
		 "                SELECT Message.id\n"
		 "                FROM Message, MessageTarget, Network\n"
		 "                WHERE Message.target = MessageTarget.id\n"
		 "                AND MessageTarget.network = Network.id\n"
		 "                AND Network.user = ?\n"
		 "            )"),
		("DELETE FROM MessageTarget\n"
		 "            WHERE id IN (\n"
		 "                SELECT MessageTarget.id\n"
		 "                FROM MessageTarget, Network\n"
		 "                WHERE MessageTarget.network = Network.id\n"
		 "                AND Network.user = ?\n"
		 "            )"),
		"DELETE FROM WebPushSubscription WHERE user = ?",
		"DELETE FROM DeviceCertificate WHERE user = ?",
		("DELETE FROM Channel\n"
		 "            WHERE id IN (\n"
		 "                SELECT Channel.id\n"
		 "                FROM Channel\n"
		 "                JOIN Network ON Channel.network = Network.id\n"
		 "                WHERE Network.user = ?\n"
		 "            )"),
		"DELETE FROM Network WHERE user = ?",
		"DELETE FROM User WHERE id = ?",
	};
	if (!db_begin(dbc, err)) {
		return false;
	}
	for (size_t i = 0; i < sizeof(deletes) / sizeof(deletes[0]); i++) {
		bool ok = deletes[i] != nullptr ? db_exec_i64(dbc, deletes[i], id, err)
						: record_message_id_high_water(dbc, err);
		if (!ok) {
			db_rollback(dbc);
			return false;
		}
	}
	if (!db_commit(dbc, err)) {
		db_rollback(dbc);
		return false;
	}
	return true;
}

static bool cert_from_row(struct db_stmt *st, int first, struct db_device_certificate *c,
			  struct err **err) {
	// id, label, [fingerprint,] last_used, last_ip
	*c = (struct db_device_certificate){.id = col_i64(st, first)};
	bool ok = col_text(st, first + 1, &c->label, err);
	return ok;
}

bool db_get_device_certificate(struct db *db, struct str fingerprint, int64_t *user,
			       struct db_device_certificate *out, bool *found, struct err **err) {
	struct db_conn *dbc = db->account;
	*found = false;
	struct db_stmt *st = db_prepare(dbc,
					"SELECT user, id, label, last_used, last_ip\n"
					"                FROM DeviceCertificate\n"
					"                WHERE fingerprint = ?",
					err);
	if (st == nullptr) {
		return false;
	}
	bind_pos_blob(st, 1, fingerprint.p, fingerprint.len);
	int rc = db_step(st);
	bool ok = true;
	if (rc == SQLITE_ROW) {
		struct db_device_certificate c;
		ok = cert_from_row(st, 1, &c, err);
		ok = ok && col_time(st, 3, &c.last_used, err);
		ok = ok && col_text(st, 4, &c.last_ip, err);
		if (ok) {
			strbuf_append_str(&c.fingerprint, fingerprint);
			strbuf_append(&c.fingerprint, "", 0);
			*user = col_i64(st, 0);
			*out = c;
			*found = true;
		} else {
			db_device_certificate_free(&c);
		}
	} else if (rc != SQLITE_DONE) {
		ok = db_sql_err(dbc, err);
	}
	db_finalize(st);
	bool ready = true;
	if (ok && *found && db->cloud != nullptr &&
	    (!(ok = turso_ready(db->cloud, *user, &ready, err)) || !ready)) {
		db_device_certificate_free(out);
		*found = false;
	}
	return ok;
}

bool db_list_device_certificates(struct db *db, int64_t user_id, db_cert_vec *out,
				 struct err **err) {
	struct db_conn *dbc = db->account;
	struct db_stmt *st = db_prepare(dbc,
					"SELECT id, label, fingerprint, last_used, last_ip\n"
					"            FROM DeviceCertificate\n"
					"            WHERE user = ?",
					err);
	if (st == nullptr) {
		return false;
	}
	bind_pos_i64(st, 1, user_id);
	int rc;
	bool ok = true;
	db_cert_vec v = {0};
	while (ok && (rc = db_step(st)) == SQLITE_ROW) {
		struct db_device_certificate c;
		ok = cert_from_row(st, 0, &c, err);
		if (ok) {
			strbuf_append_str(&c.fingerprint, col_bytes(st, 2));
			strbuf_append(&c.fingerprint, "", 0);
			ok = col_time(st, 3, &c.last_used, err) && col_text(st, 4, &c.last_ip, err);
		}
		if (ok) {
			vec_push(&v, c);
		} else {
			db_device_certificate_free(&c);
		}
	}
	if (ok && rc != SQLITE_DONE) {
		ok = db_sql_err(dbc, err);
	}
	db_finalize(st);
	if (!ok) {
		db_cert_vec_free(&v);
		return false;
	}
	for (size_t i = 0; i < v.len; i++) {
		vec_push(out, v.data[i]);
	}
	vec_free(&v);
	return true;
}

bool db_store_device_certificate(struct db *db, int64_t user_id, struct db_device_certificate *c,
				 struct err **err) {
	struct db_conn *dbc = db->account;
	struct db_stmt *st;
	if (c->id != 0) {
		st = db_prepare(
			dbc,
			"UPDATE DeviceCertificate\n"
			"                SET label = :label, fingerprint = :fingerprint, "
			"last_used = :last_used, last_ip = :last_ip\n"
			"                WHERE id = :id AND (label IS NOT :label OR fingerprint "
			"IS NOT :fingerprint OR last_used IS NOT :last_used OR last_ip IS NOT "
			":last_ip)",
			err);
		if (st == nullptr) {
			return false;
		}
		bind_i64(st, ":id", c->id);
	} else {
		st = db_prepare(
			dbc,
			"INSERT INTO DeviceCertificate(user, label, fingerprint, last_used, "
			"last_ip)\n"
			"                VALUES (:user, :label, :fingerprint, :last_used, "
			":last_ip)",
			err);
		if (st == nullptr) {
			return false;
		}
		bind_i64(st, ":user", user_id);
	}
	bind_text(st, ":label", c->label);
	bind_bytes(st, ":fingerprint", c->fingerprint.buf, c->fingerprint.len);
	bind_time(st, ":last_used", &c->last_used);
	bind_text(st, ":last_ip", c->last_ip);
	int rc = db_step(st);
	bool ok = rc == SQLITE_DONE;
	if (!ok) {
		if (db_errcode(dbc) == SQLITE_CONSTRAINT_UNIQUE) {
			err_set(err, DB_ERR_DUPLICATE_DEVICE_CERTIFICATE,
				"duplicate device certificate");
		} else {
			db_sql_err(dbc, err);
		}
	} else if (c->id == 0) {
		c->id = db_last_insert_rowid(dbc);
	}
	db_finalize(st);
	return ok;
}

bool db_delete_device_certificate(struct db *db, int64_t user_id, struct str fingerprint,
				  struct err **err) {
	struct db_conn *dbc = db->account;
	struct db_stmt *st = db_prepare(
		dbc, "DELETE FROM DeviceCertificate WHERE user = ? AND fingerprint = ?", err);
	if (st == nullptr) {
		return false;
	}
	bind_pos_i64(st, 1, user_id);
	bind_pos_blob(st, 2, fingerprint.p, fingerprint.len);
	return run(st, err);
}
