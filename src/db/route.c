// Routing of tenant queries to the owning database (soju-rs
// database/routing.rs). With local SQLite every route is the account
// connection.
#include "db/internal.h"
#include "db/turso.h"

#include "base/alloc.h"

struct db_conn *db_user_conn(struct db *db, int64_t user_id, struct err **err) {
	return db->cloud != nullptr ? turso_user(db->cloud, user_id, err) : db->account;
}

struct db_conn *db_network_conn(struct db *db, int64_t network_id, struct err **err) {
	return db->cloud != nullptr ? turso_route(db->cloud, "Network", network_id, err)
				    : db->account;
}

struct db_conn *db_channel_conn(struct db *db, int64_t id, struct err **err) {
	return db->cloud != nullptr ? turso_route(db->cloud, "Channel", id, err) : db->account;
}

struct db_conn *db_subscription_conn(struct db *db, int64_t id, struct err **err) {
	return db->cloud != nullptr ? turso_route(db->cloud, "WebPushSubscription", id, err)
				    : db->account;
}

bool db_is_remote(struct db *db) {
	return db->cloud != nullptr;
}

bool db_turso_usage(struct db *db, uint64_t *read, uint64_t *written) {
	if (db->cloud == nullptr) {
		return false;
	}
	hrana_shared_usage(db->cloud->shared, read, written);
	return true;
}

bool db_open_turso(const struct config_turso *cfg, const char *api_base, const char *sql_base,
		   struct db **out, struct err **err) {
	struct turso *t = nullptr;
	if (!turso_open(cfg, api_base, sql_base, &t, err)) {
		return false;
	}
	struct db *db = xcalloc(1, sizeof(*db));
	db->cloud = t;
	db->account = &t->controller;
	*out = db;
	return true;
}
