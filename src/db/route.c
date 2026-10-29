// Routing of tenant queries to the owning database (soju-rs
// database/routing.rs). With local SQLite every route is the account
// connection.
#include "db/internal.h"

struct db_conn *db_user_conn(struct db *db, int64_t user_id, struct err **err) {
	(void)user_id, (void)err;
	return &db->account;
}

struct db_conn *db_network_conn(struct db *db, int64_t network_id, struct err **err) {
	(void)network_id, (void)err;
	return &db->account;
}

struct db_conn *db_channel_conn(struct db *db, int64_t id, struct err **err) {
	(void)id, (void)err;
	return &db->account;
}

struct db_conn *db_subscription_conn(struct db *db, int64_t id, struct err **err) {
	(void)id, (void)err;
	return &db->account;
}
