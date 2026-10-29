// Turso Cloud storage (soju-rs database/turso/mod.rs): a controller
// database holding accounts, and one database per user holding everything
// else, provisioned through Turso's Platform API. Links are recoverable:
// a user is reserved in the controller before its database exists, and
// deletions are recorded before anything is deleted, so startup resumes
// both.
#pragma once

#include <pthread.h>

#include "config/config.h"
#include "db/conn.h"
#include "db/hrana.h"

struct tenant {
	int64_t user;
	char *name;
	struct db_conn conn;
};

struct turso_route {
	const char *table; // static
	int64_t id, user;
};

struct turso {
	struct config_turso cfg;
	char *api_base;
	char *sql_base; // tests: the SQL host of every database, else nullptr
	struct hrana_shared *shared;
	struct db_conn controller;
	// Tenants are opened and dropped on the DB thread; message writers
	// fork their connections from other threads
	pthread_mutex_t mu;
	VEC(struct tenant *) users;
	VEC(struct turso_route) routes;
};

// Opens the controller (creating it if needed) and every user database.
// api_base and sql_base are for tests (nullptr: Turso's).
[[nodiscard]] bool turso_open(const struct config_turso *cfg, const char *api_base,
			      const char *sql_base, struct turso **out, struct err **err);
void turso_close(struct turso *t);

// Whether a user's database is ready (users being provisioned or deleted
// aren't loaded)
[[nodiscard]] bool turso_ready(struct turso *t, int64_t id, bool *ready, struct err **err);
// Reserves a user in the controller, then provisions its database; sets
// u->id
[[nodiscard]] bool turso_create_user(struct turso *t, struct db_user *u, struct err **err);
[[nodiscard]] bool turso_delete_user(struct turso *t, int64_t id, struct err **err);
// A user's database connection, or nullptr
[[nodiscard]] struct db_conn *turso_user(struct turso *t, int64_t id, struct err **err);
// The database holding a row of a tenant table (cached, else probed)
[[nodiscard]] struct db_conn *turso_route(struct turso *t, const char *table, int64_t id,
					  struct err **err);
[[nodiscard]] bool turso_remember_network(struct turso *t, int64_t user, int64_t network,
					  struct err **err);
// A new connection to a user's database, on its own stream (message
// writers); nullptr if the user is unknown
[[nodiscard]] struct hrana *turso_fork_user(struct turso *t, int64_t id, struct err **err);

// 128 random bits as 25 base36 characters (soju-rs database_name)
[[nodiscard]] char *turso_database_name(void);

// db/tenant.c: the schemas of remote databases (soju-rs sqlite/turso.rs)
[[nodiscard]] bool turso_init_controller(struct db_conn *c, struct err **err);
[[nodiscard]] bool turso_init_tenant(struct db_conn *c, const char *controller, int64_t owner,
				     struct err **err);
