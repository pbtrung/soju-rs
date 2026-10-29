#include "auth/auth.h"
#include "auth/internal.h"

#include <string.h>

#include "base/alloc.h"

void auth_error_free(struct auth_error *e) {
	xfree(e->internal);
	xfree(e->external);
	*e = (struct auth_error){0};
}

// One internal authentication, through the DB thread and the CPU limiter:
// get_user, then the bcrypt check, then the hash upgrade if any
struct plain_job {
	struct server *srv;
	char *username;
	char *password;
	auth_done_fn done;
	void *data;
	// results
	bool found;
	struct db_user user;
	char *old_hash;
	char *db_err;
	bool upgraded;
	char *check_err; // invalid credentials
};

static void job_free(struct plain_job *j) {
	xfree(j->username);
	if (j->password != nullptr) {
		explicit_bzero(j->password, strlen(j->password));
	}
	xfree(j->password);
	if (j->found) {
		db_user_free(&j->user);
	}
	xfree(j->old_hash);
	xfree(j->db_err);
	xfree(j->check_err);
	xfree(j);
}

static void finish(struct plain_job *j, const char *internal, bool invalid_credentials) {
	struct auth_error e = {0};
	if (internal != nullptr) {
		e.internal = xstrdup(internal);
		e.external = xstrdup(invalid_credentials ? "Invalid credentials"
							 : "Authentication failed");
	}
	j->done(j->data, internal != nullptr ? &e : nullptr);
	auth_error_free(&e);
	job_free(j);
}

static void run_upgrade(struct db *db, void *data) {
	struct plain_job *j = data;
	struct err *e = nullptr;
	if (!db_upgrade_user_password(db, j->user.id, j->old_hash, j->user.password, &e)) {
		j->db_err = xstrdup(err_msg(e));
		err_free(e);
	}
}

static void done_upgrade(void *data) {
	struct plain_job *j = data;
	finish(j, j->db_err, false);
}

static void run_check(void *data) {
	struct plain_job *j = data;
	struct err *e = nullptr;
	if (!db_user_check_password(&j->user, j->password, &j->upgraded, &e)) {
		j->check_err = xstrdup(err_msg(e));
		err_free(e);
	}
}

static void done_check(void *data, const char *err) {
	struct plain_job *j = data;
	if (err != nullptr) {
		finish(j, err, false);
		return;
	}
	if (j->check_err != nullptr) {
		finish(j, j->check_err, true);
		return;
	}
	if (j->upgraded) {
		// Only the hash: the rest of the record may be updated meanwhile by
		// the user's event loop
		server_db(j->srv, run_upgrade, done_upgrade, j);
		return;
	}
	finish(j, nullptr, false);
}

static void run_get_user(struct db *db, void *data) {
	struct plain_job *j = data;
	struct err *e = nullptr;
	if (!db_get_user(db, j->username, &j->user, &j->found, &e)) {
		j->db_err = xstrdup(err_msg(e));
		err_free(e);
	}
}

static void done_get_user(void *data) {
	struct plain_job *j = data;
	if (j->db_err != nullptr) {
		finish(j, j->db_err, false);
		return;
	}
	if (!j->found) {
		finish(j, "user not found: sql: no rows in result set", true);
		return;
	}
	j->old_hash = xstrdup(j->user.password);
	server_cpu(j->srv, run_check, done_check, j);
}

void auth_plain(struct server *srv, const char *username, const char *password,
		const char *remote_addr, auth_done_fn done, void *data) {
	struct server_config *cfg = server_config_get(srv);
	enum plain_auth kind = cfg->plain_auth;
	if (kind == PLAIN_AUTH_HTTP) {
		auth_plain_http(srv, cfg->http_auth_url, username, password, remote_addr, done,
				data);
		server_config_unref(cfg);
		return;
	}
	if (kind == PLAIN_AUTH_OAUTH2 && cfg->oauth2 != nullptr) {
		auth_plain_oauth2(srv, cfg->oauth2, username, password, remote_addr, done, data);
		server_config_unref(cfg);
		return;
	}
	server_config_unref(cfg);
	if (kind != PLAIN_AUTH_INTERNAL) {
		struct auth_error e = {
			.internal = xstrdup("SASL PLAIN not supported"),
			.external = xstrdup("Authentication failed"),
		};
		done(data, &e);
		auth_error_free(&e);
		return;
	}
	struct plain_job *j = xcalloc(1, sizeof(*j));
	*j = (struct plain_job){
		.srv = srv,
		.username = xstrdup(username),
		.password = xstrdup(password),
		.done = done,
		.data = data,
	};
	server_db(srv, run_get_user, done_get_user, j);
}
