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
	struct db_conn local; // local SQLite
	struct db_conn *account; // local, or the Turso controller
	struct turso *cloud; // nullptr for local SQLite
};

// The connection holding a user's, network's, channel's or Web Push
// subscription's data; nullptr (and *err set) if unknown
[[nodiscard]] struct db_conn *db_user_conn(struct db *db, int64_t user_id, struct err **err);
[[nodiscard]] struct db_conn *db_network_conn(struct db *db, int64_t network_id, struct err **err);
[[nodiscard]] struct db_conn *db_channel_conn(struct db *db, int64_t id, struct err **err);
[[nodiscard]] struct db_conn *db_subscription_conn(struct db *db, int64_t id, struct err **err);

[[nodiscard]] bool record_message_id_high_water(struct db_conn *c, struct err **err);
// The stored time and searchable text of a message (text: nullptr for
// none)
[[nodiscard]] bool message_time_text(const struct irc_msg *msg, char **time, char **text,
				     struct err **err);

// db.c: schemas and migrations of a connection (remote ones keep their
// version in SojuSchemaVersion)
[[nodiscard]] bool db_conn_upgrade(struct db_conn *c, struct err **err);
[[nodiscard]] bool db_conn_schema_version(struct db_conn *c, int64_t *out, struct err **err);
[[nodiscard]] bool db_conn_set_schema_version(struct db_conn *c, int64_t v, struct err **err);
[[nodiscard]] const char *db_fts_schema(void);
[[nodiscard]] const char *db_tenant_schema(void);
