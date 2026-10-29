// Shared by the src/db/*.c files: the routing database and its
// connections (soju-rs database/routing.rs)
#pragma once

#include "db/conn.h"
#include "db/db.h"

struct turso;

// Account data (users, certificates, Web Push keys) lives in the account
// connection: the local database, or Turso's controller. Tenant data
// (networks and what belongs to them) lives in the owning user's
// database, which is the account connection too for local SQLite.
struct db {
	struct db_conn account;
	struct turso *cloud; // nullptr for local SQLite
};

// The connection holding a user's, network's, channel's or Web Push
// subscription's data; nullptr (and *err set) if unknown
[[nodiscard]] struct db_conn *db_user_conn(struct db *db, int64_t user_id, struct err **err);
[[nodiscard]] struct db_conn *db_network_conn(struct db *db, int64_t network_id, struct err **err);
[[nodiscard]] struct db_conn *db_channel_conn(struct db *db, int64_t id, struct err **err);
[[nodiscard]] struct db_conn *db_subscription_conn(struct db *db, int64_t id, struct err **err);

[[nodiscard]] bool record_message_id_high_water(struct db_conn *c, struct err **err);
