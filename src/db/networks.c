// Networks and their channels (soju-rs database/sqlite/networks.rs)
#include <math.h>

#include "base/alloc.h"
#include "base/utf8.h"
#include "db/internal.h"

bool db_list_networks(struct db *db, int64_t user_id, db_network_vec *out, struct err **err) {
	sqlite3_stmt *st = db_prepare(
		db,
		"SELECT id, name, addr, nick, username, realname, certfp, pass,\n"
		"                connect_commands, sasl_mechanism, sasl_plain_username, "
		"sasl_plain_password,\n"
		"                sasl_external_cert, sasl_external_key, auto_away, enabled\n"
		"            FROM Network\n"
		"            WHERE user = ?",
		err);
	if (st == nullptr) {
		return false;
	}
	sqlite3_bind_int64(st, 1, user_id);
	db_network_vec v = {0};
	int rc;
	bool ok = true;
	while (ok && (rc = sqlite3_step(st)) == SQLITE_ROW) {
		struct db_network n = {.id = col_i64(st, 0)};
		ok = col_text(st, 2, &n.addr, err);
		n.name = col_lossy(st, 1);
		n.nick = col_lossy(st, 3);
		n.username = col_lossy(st, 4);
		n.realname = col_lossy(st, 5);
		n.certfp = col_lossy(st, 6);
		n.pass = col_lossy(st, 7);
		AUTOFREE char *cc = col_lossy(st, 8);
		if (cc[0] != '\0') {
			struct str rest = str_from(cc), before, after;
			while (str_split_once_str(rest, STR_LIT("\r\n"), &before, &after)) {
				strvec_push_take(&n.connect_commands, str_dup(before));
				rest = after;
			}
			strvec_push_take(&n.connect_commands, str_dup(rest));
		}
		n.sasl.mechanism = col_lossy(st, 9);
		n.sasl.plain_username = col_lossy(st, 10);
		n.sasl.plain_password = col_lossy(st, 11);
		col_blob(st, 12, &n.sasl.external_cert);
		col_blob(st, 13, &n.sasl.external_key);
		n.auto_away = col_bool(st, 14);
		n.enabled = col_bool(st, 15);
		if (ok) {
			vec_push(&v, n);
		} else {
			db_network_free(&n);
		}
	}
	if (ok && rc != SQLITE_DONE) {
		ok = db_sql_err(db, err);
	}
	sqlite3_finalize(st);
	if (!ok) {
		db_network_vec_free(&v);
		return false;
	}
	for (size_t i = 0; i < v.len; i++) {
		vec_push(out, v.data[i]);
	}
	vec_free(&v);
	return true;
}

bool db_store_network(struct db *db, int64_t user_id, struct db_network *n, struct err **err) {
	const char *mech = nullptr, *plain_user = nullptr, *plain_pass = nullptr;
	if (n->sasl.mechanism[0] != '\0') {
		mech = n->sasl.mechanism;
		if (cstr_eq(mech, "PLAIN")) {
			plain_user = n->sasl.plain_username;
			plain_pass = n->sasl.plain_password;
			strbuf_free(&n->sasl.external_cert.data);
			n->sasl.external_cert.set = false;
			strbuf_free(&n->sasl.external_key.data);
			n->sasl.external_key.set = false;
		} else if (!cstr_eq(mech, "EXTERNAL")) {
			AUTOFREE char *q = utf8_debug_cstr(str_from(mech));
			return err_set(err, 0,
				       "soju: cannot store network: unsupported SASL mechanism %s",
				       q);
		}
	}
	AUTO_STRBUF struct strbuf cc = STRBUF_INIT;
	for (size_t i = 0; i < n->connect_commands.len; i++) {
		if (i > 0) {
			strbuf_append_cstr(&cc, "\r\n");
		}
		strbuf_append_cstr(&cc, n->connect_commands.data[i]);
	}

	sqlite3_stmt *st;
	if (n->id != 0) {
		st = db_prepare(
			db,
			"UPDATE Network\n"
			"                SET name = :name, addr = :addr, nick = :nick, username = "
			":username,\n"
			"                    realname = :realname, certfp = :certfp, pass = :pass, "
			"connect_commands = :connect_commands,\n"
			"                    sasl_mechanism = :sasl_mechanism, sasl_plain_username "
			"= "
			":sasl_plain_username, sasl_plain_password = :sasl_plain_password,\n"
			"                    sasl_external_cert = :sasl_external_cert, "
			"sasl_external_key = :sasl_external_key,\n"
			"                    auto_away = :auto_away, enabled = :enabled\n"
			"                WHERE id = :id AND (name IS NOT :name OR addr IS NOT "
			":addr OR "
			"nick IS NOT :nick OR username IS NOT :username OR realname IS NOT "
			":realname OR "
			"certfp IS NOT :certfp OR pass IS NOT :pass OR connect_commands IS NOT "
			":connect_commands OR sasl_mechanism IS NOT :sasl_mechanism OR "
			"sasl_plain_username IS NOT :sasl_plain_username OR sasl_plain_password IS "
			"NOT "
			":sasl_plain_password OR sasl_external_cert IS NOT :sasl_external_cert OR "
			"sasl_external_key IS NOT :sasl_external_key OR auto_away IS NOT "
			":auto_away OR "
			"enabled IS NOT :enabled)",
			err);
		if (st == nullptr) {
			return false;
		}
		bind_i64(st, ":id", n->id);
	} else {
		st = db_prepare(
			db,
			"INSERT INTO Network(id, user, name, addr, nick, username, realname, "
			"certfp, "
			"pass,\n"
			"                    connect_commands, sasl_mechanism, "
			"sasl_plain_username,\n"
			"                    sasl_plain_password, sasl_external_cert, "
			"sasl_external_key, auto_away, enabled)\n"
			"                VALUES ((SELECT max(coalesce((SELECT max(id) FROM "
			"Network),0),coalesce((SELECT last_id FROM ActorIdHighWater WHERE "
			"kind='network'),0))+1), :user, :name, :addr, :nick, :username, "
			":realname, :certfp, :pass,\n"
			"                    :connect_commands, :sasl_mechanism, "
			":sasl_plain_username,\n"
			"                    :sasl_plain_password, :sasl_external_cert, "
			":sasl_external_key, :auto_away, :enabled)",
			err);
		if (st == nullptr) {
			return false;
		}
		bind_i64(st, ":user", user_id);
	}
	bind_text_or_null(st, ":name", n->name);
	bind_text(st, ":addr", n->addr);
	bind_text_or_null(st, ":nick", n->nick);
	bind_text_or_null(st, ":username", n->username);
	bind_text_or_null(st, ":realname", n->realname);
	bind_text_or_null(st, ":certfp", n->certfp);
	bind_text_or_null(st, ":pass", n->pass);
	bind_text_or_null(st, ":connect_commands", strbuf_cstr(&cc));
	if (mech != nullptr) {
		bind_text(st, ":sasl_mechanism", mech);
	} else {
		bind_null(st, ":sasl_mechanism");
	}
	if (plain_user != nullptr) {
		bind_text_or_null(st, ":sasl_plain_username", plain_user);
		bind_text_or_null(st, ":sasl_plain_password", plain_pass);
	} else {
		bind_null(st, ":sasl_plain_username");
		bind_null(st, ":sasl_plain_password");
	}
	bind_blob(st, ":sasl_external_cert", &n->sasl.external_cert);
	bind_blob(st, ":sasl_external_key", &n->sasl.external_key);
	bind_i64(st, ":auto_away", n->auto_away);
	bind_i64(st, ":enabled", n->enabled);
	bool ok = db_step_done(db, st, err);
	sqlite3_finalize(st);
	if (ok && n->id == 0) {
		n->id = sqlite3_last_insert_rowid(db->conn);
	}
	return ok;
}

bool db_delete_network(struct db *db, int64_t id, struct err **err) {
	static const char *const deletes[] = {
		("DELETE FROM Message WHERE target IN (SELECT id FROM MessageTarget WHERE network "
		 "= "
		 "?)"),
		"DELETE FROM MessageTarget WHERE network = ?",
		"DELETE FROM WebPushSubscription WHERE network = ?",
		"DELETE FROM DeliveryReceipt WHERE network = ?",
		"DELETE FROM ReadReceipt WHERE network = ?",
		"DELETE FROM Channel WHERE network = ?",
		"DELETE FROM Network WHERE id = ?",
	};
	if (!db_begin(db, err)) {
		return false;
	}
	bool ok = record_message_id_high_water(db, err);
	for (size_t i = 0; ok && i < sizeof(deletes) / sizeof(deletes[0]); i++) {
		ok = db_exec_i64(db, deletes[i], id, err);
	}
	if (!ok || !db_commit(db, err)) {
		db_rollback(db);
		return false;
	}
	return true;
}

bool db_list_channels(struct db *db, int64_t network_id, db_channel_vec *out, struct err **err) {
	sqlite3_stmt *st =
		db_prepare(db,
			   "SELECT\n"
			   "                id, name, key, detached, detached_internal_msgid,\n"
			   "                relay_detached, reattach_on, detach_after, "
			   "detach_on\n"
			   "            FROM Channel\n"
			   "            WHERE network = ?",
			   err);
	if (st == nullptr) {
		return false;
	}
	sqlite3_bind_int64(st, 1, network_id);
	int rc;
	while ((rc = sqlite3_step(st)) == SQLITE_ROW) {
		int64_t detach_after = col_i64(st, 7);
		uint64_t secs = detach_after > 0 ? (uint64_t)detach_after : 0;
		struct db_channel c = {
			.id = col_i64(st, 0),
			.name = col_lossy(st, 1),
			.key = col_lossy(st, 2),
			.detached = col_bool(st, 3),
			.detached_internal_msg_id = col_lossy(st, 4),
			.relay_detached = message_filter_from_i64(col_i64(st, 5)),
			.reattach_on = message_filter_from_i64(col_i64(st, 6)),
			.detach_after_ns =
				secs > UINT64_MAX / 1000000000 ? UINT64_MAX : secs * 1000000000,
			.detach_on = message_filter_from_i64(col_i64(st, 8)),
		};
		vec_push(out, c);
	}
	bool ok = rc == SQLITE_DONE || db_sql_err(db, err);
	sqlite3_finalize(st);
	return ok;
}

bool db_store_channel(struct db *db, int64_t network_id, struct db_channel *c, struct err **err) {
	// as_secs_f64().ceil()
	int64_t detach_after = (int64_t)ceil((double)c->detach_after_ns / 1e9);
	sqlite3_stmt *st;
	if (c->id != 0) {
		st = db_prepare(
			db,
			"UPDATE Channel\n"
			"                SET network = :network, name = :name, key = :key, "
			"detached = "
			":detached,\n"
			"                    detached_internal_msgid = :detached_internal_msgid, "
			"relay_detached = :relay_detached,\n"
			"                    reattach_on = :reattach_on, detach_after = "
			":detach_after, "
			"detach_on = :detach_on\n"
			"                WHERE id = :id AND (network IS NOT :network OR name IS "
			"NOT "
			":name OR key IS NOT :key OR detached IS NOT :detached OR "
			"detached_internal_msgid IS NOT :detached_internal_msgid OR relay_detached "
			"IS "
			"NOT :relay_detached OR reattach_on IS NOT :reattach_on OR detach_after IS "
			"NOT "
			":detach_after OR detach_on IS NOT :detach_on)",
			err);
		if (st == nullptr) {
			return false;
		}
		bind_i64(st, ":id", c->id);
	} else {
		st = db_prepare(
			db,
			"INSERT INTO Channel(id, network, name, key, detached, "
			"detached_internal_msgid, relay_detached, reattach_on, detach_after, "
			"detach_on)\n"
			"                VALUES (:id, :network, :name, :key, :detached, "
			":detached_internal_msgid, :relay_detached, :reattach_on, "
			":detach_after, :detach_on)",
			err);
		if (st == nullptr) {
			return false;
		}
		bind_null(st, ":id");
	}
	bind_i64(st, ":network", network_id);
	bind_text(st, ":name", c->name);
	bind_text_or_null(st, ":key", c->key);
	bind_i64(st, ":detached", c->detached);
	bind_text_or_null(st, ":detached_internal_msgid", c->detached_internal_msg_id);
	bind_i64(st, ":relay_detached", c->relay_detached);
	bind_i64(st, ":reattach_on", c->reattach_on);
	bind_i64(st, ":detach_after", detach_after);
	bind_i64(st, ":detach_on", c->detach_on);
	bool ok = db_step_done(db, st, err);
	sqlite3_finalize(st);
	if (ok && c->id == 0) {
		c->id = sqlite3_last_insert_rowid(db->conn);
	}
	return ok;
}

bool db_delete_channel(struct db *db, int64_t id, struct err **err) {
	return db_exec_i64(db, "DELETE FROM Channel WHERE id = ?", id, err);
}
