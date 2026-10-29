#include "db/turso.h"

#include <ctype.h>
#include <openssl/rand.h>
#include <stdlib.h>
#include <unistd.h>
#include <yyjson.h>

#include "base/alloc.h"
#include "base/timeutil.h"
#include "http/response.h"

static const char TURSO_API[] = "https://api.turso.tech/v1";

// 128-bit numbers as 4 32-bit limbs, most significant first

// n = n / d, returning the remainder
static uint32_t divmod(uint32_t n[4], uint32_t d) {
	uint64_t rem = 0;
	for (size_t i = 0; i < 4; i++) {
		uint64_t cur = (rem << 32U) | n[i];
		n[i] = (uint32_t)(cur / d);
		rem = cur % d;
	}
	return (uint32_t)rem;
}

// n = n * m + a; false on overflow
static bool muladd(uint32_t n[4], uint32_t m, uint32_t a) {
	uint64_t carry = a;
	for (size_t i = 4; i-- > 0;) {
		uint64_t cur = (uint64_t)n[i] * m + carry;
		n[i] = (uint32_t)cur;
		carry = cur >> 32U;
	}
	return carry == 0;
}

char *turso_database_name(void) {
	uint8_t bytes[16];
	if (RAND_bytes(bytes, sizeof(bytes)) != 1) {
		abort();
	}
	uint32_t n[4];
	for (size_t i = 0; i < 4; i++) {
		n[i] = ((uint32_t)bytes[4 * i] << 24U) | ((uint32_t)bytes[4 * i + 1] << 16U) |
		       ((uint32_t)bytes[4 * i + 2] << 8U) | bytes[4 * i + 3];
	}
	// Fixed width: padding retains the full entropy, leading zeroes
	// included
	static const char alphabet[] = "0123456789abcdefghijklmnopqrstuvwxyz";
	char name[26];
	name[25] = '\0';
	for (int i = 24; i >= 0; i--) {
		name[i] = alphabet[divmod(n, 36)];
	}
	return xstrdup(name);
}

static void status_text(struct strbuf *out, long status) {
	http_status_text(out, (int)status);
}

// A Platform API or SQL request: *status, and the JSON body of successes
static bool request(struct turso *t, const char *method, const char *url, const char *token,
		    yyjson_mut_doc *body, long *status, yyjson_doc **out, struct err **err) {
	size_t len = 0;
	char *json = body != nullptr ? yyjson_mut_write(body, 0, &len) : nullptr;
	bool ok = hrana_http(t->shared, method, url, token, json, len, status, out, err);
	free(json);
	return ok;
}

// soju-rs Http::json: a successful status is required
static bool request_json(struct turso *t, const char *method, const char *url, const char *token,
			 yyjson_doc **out, struct err **err) {
	long status = 0;
	if (!request(t, method, url, token, nullptr, &status, out, err)) {
		return false;
	}
	if (status < 200 || status > 299) {
		AUTO_STRBUF struct strbuf st = STRBUF_INIT;
		status_text(&st, status);
		return err_set(err, 0, "Turso request failed (HTTP %s)", strbuf_cstr(&st));
	}
	return true;
}

static bool valid_hostname(const char *host) {
	if (!str_ends_with(str_from(host), STR_LIT(".turso.io"))) {
		return false;
	}
	for (const char *c = host; *c != '\0'; c++) {
		if (!isalnum((unsigned char)*c) && *c != '-' && *c != '.') {
			return false;
		}
	}
	return true;
}

// Waits for a new database to serve SQL. Only this read is retried: writes
// are never replayed after an ambiguous HTTP failure.
static bool probe(struct turso *t, const char *name, const char *endpoint, struct err **err) {
	static const char body[] =
		"{\"requests\":[{\"type\":\"execute\",\"stmt\":{\"sql\":\"SELECT "
		"1\"}},{\"type\":\"close\"}]}";
	for (int attempt = 0; attempt < 6; attempt++) {
		long status = 0;
		yyjson_doc *doc = nullptr;
		struct err *e = nullptr;
		bool ok = hrana_http(t->shared, "POST", endpoint, t->cfg.group_token, body,
				     sizeof(body) - 1, &status, &doc, &e);
		if (ok && status >= 200 && status <= 299) {
			yyjson_val *r0 = yyjson_arr_get(
				yyjson_obj_get(yyjson_doc_get_root(doc), "results"), 0);
			const char *type = yyjson_get_str(yyjson_obj_get(r0, "type"));
			bool ready = type != nullptr && cstr_eq(type, "ok");
			yyjson_doc_free(doc);
			if (ready) {
				return true;
			}
		} else {
			yyjson_doc_free(doc);
		}
		if (ok && (status == 401 || status == 403)) {
			return err_set(err, 0, "Turso SQL authorization failed for database %s",
				       name);
		}
		if (attempt == 5) {
			if (!ok) {
				err_set(err, 0, "Turso database %s did not become ready: %s", name,
					err_msg(e));
				err_free(e);
				return false;
			}
			AUTO_STRBUF struct strbuf st = STRBUF_INIT;
			status_text(&st, status);
			return err_set(err, 0, "Turso database %s did not become ready (HTTP %s)",
				       name, strbuf_cstr(&st));
		}
		err_free(e);
		usleep(100000U << (unsigned)attempt);
	}
	return false;
}

// soju-rs ensure_database: the pipeline endpoint of a database, created if
// allowed
static bool ensure_database(struct turso *t, const char *name, bool allow_create, char **endpoint,
			    struct err **err) {
	AUTOFREE char *collection =
		xasprintf("%s/organizations/%s/databases", t->api_base, t->cfg.organization);
	AUTOFREE char *url = xasprintf("%s/%s", collection, name);
	long status = 0;
	yyjson_doc *doc = nullptr;
	if (!request(t, "GET", url, t->cfg.api_token, nullptr, &status, &doc, err)) {
		return false;
	}
	if (status == 404) {
		if (!allow_create) {
			return err_set(err, 0,
				       "linked Turso database %s is missing; restore it before "
				       "restarting soju (refusing to recreate it empty)",
				       name);
		}
		yyjson_mut_doc *body = yyjson_mut_doc_new(nullptr);
		yyjson_mut_val *root = yyjson_mut_obj(body);
		yyjson_mut_doc_set_root(body, root);
		yyjson_mut_obj_add_strcpy(body, root, "name", name);
		yyjson_mut_obj_add_strcpy(body, root, "group", t->cfg.group);
		long created = 0;
		yyjson_doc *ignored = nullptr;
		bool ok = request(t, "POST", collection, t->cfg.api_token, body, &created, &ignored,
				  err);
		yyjson_mut_doc_free(body);
		yyjson_doc_free(ignored);
		if (!ok) {
			return false;
		}
		// A concurrent starter may have created the same controller
		if ((created < 200 || created > 299) && created != 409) {
			AUTO_STRBUF struct strbuf st = STRBUF_INIT;
			status_text(&st, created);
			return err_set(err, 0, "create Turso database %s: HTTP %s", name,
				       strbuf_cstr(&st));
		}
		if (!request_json(t, "GET", url, t->cfg.api_token, &doc, err)) {
			return false;
		}
	} else if (status < 200 || status > 299) {
		AUTO_STRBUF struct strbuf st = STRBUF_INIT;
		status_text(&st, status);
		return err_set(err, 0, "retrieve Turso database %s: HTTP %s", name,
			       strbuf_cstr(&st));
	}
	yyjson_val *db = yyjson_obj_get(yyjson_doc_get_root(doc), "database");
	const char *group = yyjson_get_str(yyjson_obj_get(db, "group"));
	const char *host = yyjson_get_str(yyjson_obj_get(db, "Hostname"));
	bool ok = true;
	if (group == nullptr || !cstr_eq(group, t->cfg.group)) {
		ok = err_set(err, 0, "Turso database %s does not belong to configured group %s",
			     name, t->cfg.group);
	} else if (host == nullptr) {
		ok = err_set(err, 0, "Turso database %s has no hostname", name);
	} else if (t->sql_base != nullptr) {
		*endpoint = xasprintf("%s/db/%s/v2/pipeline", t->sql_base, name);
	} else if (!valid_hostname(host)) {
		ok = err_set(err, 0, "invalid Turso database hostname");
	} else {
		*endpoint = xasprintf("https://%s/v2/pipeline", host);
	}
	yyjson_doc_free(doc);
	if (ok && !probe(t, name, *endpoint, err)) {
		xfree(*endpoint);
		*endpoint = nullptr;
		return false;
	}
	return ok;
}

static void tenant_free(struct tenant *u) {
	db_conn_finish(&u->conn);
	xfree(u->name);
	xfree(u);
}

static bool provision(struct turso *t, int64_t id, const char *name, bool pending,
		      struct err **err) {
	AUTOFREE char *url = nullptr;
	if (!ensure_database(t, name, pending, &url, err)) {
		return false;
	}
	struct tenant *u = xcalloc(1, sizeof(*u));
	u->user = id;
	u->name = xstrdup(name);
	db_conn_init_remote(&u->conn, hrana_new(url, t->cfg.group_token, t->shared));
	struct err *e = nullptr;
	if (!turso_init_tenant(&u->conn, t->cfg.controller, id, &e)) {
		err_set(err, 0, "initialize user database %s: %s", name, err_msg(e));
		err_free(e);
		tenant_free(u);
		return false;
	}
	if (pending &&
	    !db_exec_i64(&t->controller, "UPDATE UserDatabase SET state = 'ready' WHERE user = ?",
			 id, err)) {
		tenant_free(u);
		return false;
	}
	pthread_mutex_lock(&t->mu);
	vec_push(&t->users, u);
	pthread_mutex_unlock(&t->mu);
	return true;
}

static bool username_exists(struct turso *t, int64_t id, bool *exists, struct err **err) {
	struct db_stmt *st = db_prepare(&t->controller, "SELECT 1 FROM User WHERE id = ?", err);
	if (st == nullptr) {
		return false;
	}
	bind_pos_i64(st, 1, id);
	int rc = db_step(st);
	*exists = rc == SQLITE_ROW;
	bool ok = rc == SQLITE_ROW || rc == SQLITE_DONE || db_sql_err(&t->controller, err);
	db_finalize(st);
	return ok;
}

// Fails unless a link names a valid user database of this controller
static bool validate_link(struct turso *t, int64_t id, const char *name, struct err **err) {
	bool valid = id > 0 && !cstr_eq(name, t->cfg.controller) && strlen(name) == 25;
	// Fits in 128 bits: 25 base36 digits may not
	uint32_t n[4] = {0};
	for (const char *c = name; valid && *c != '\0'; c++) {
		uint32_t d;
		if (*c >= '0' && *c <= '9') {
			d = (uint32_t)(*c - '0');
		} else if (*c >= 'a' && *c <= 'z') {
			d = (uint32_t)(*c - 'a' + 10);
		} else {
			valid = false;
			break;
		}
		valid = muladd(n, 36, d);
	}
	bool exists = false;
	if (valid && !username_exists(t, id, &exists, err)) {
		return false;
	}
	if (!valid || !exists) {
		return err_set(err, 0, "invalid Turso database link for user %lld", (long long)id);
	}
	return true;
}

bool turso_open(const struct config_turso *cfg, const char *api_base, const char *sql_base,
		struct turso **out, struct err **err) {
	struct turso *t = xcalloc(1, sizeof(*t));
	t->cfg = (struct config_turso){.organization = xstrdup(cfg->organization),
				       .group = xstrdup(cfg->group),
				       .controller = xstrdup(cfg->controller),
				       .group_token = xstrdup(cfg->group_token),
				       .api_token = xstrdup(cfg->api_token)};
	t->api_base = xstrdup(api_base != nullptr ? api_base : TURSO_API);
	t->sql_base = sql_base != nullptr ? xstrdup(sql_base) : nullptr;
	t->shared = hrana_shared_new();
	pthread_mutex_init(&t->mu, nullptr);
	AUTOFREE char *url = nullptr;
	struct err *e = nullptr;
	if (!ensure_database(t, t->cfg.controller, true, &url, err)) {
		goto fail;
	}
	db_conn_init_remote(&t->controller, hrana_new(url, t->cfg.group_token, t->shared));
	if (!turso_init_controller(&t->controller, &e)) {
		err_set(err, 0, "initialize Turso controller: %s", err_msg(e));
		err_free(e);
		goto fail;
	}
	struct db_stmt *st =
		db_prepare(&t->controller, "SELECT user, name, state FROM UserDatabase", err);
	if (st == nullptr) {
		goto fail;
	}
	struct link {
		int64_t id;
		char *name, *state;
	};
	VEC(struct link) links = {0};
	int rc;
	while ((rc = db_step(st)) == SQLITE_ROW) {
		vec_push(&links,
			 ((struct link){col_i64(st, 0), col_lossy(st, 1), col_lossy(st, 2)}));
	}
	bool ok = rc == SQLITE_DONE || db_sql_err(&t->controller, err);
	db_finalize(st);
	for (size_t i = 0; ok && i < links.len; i++) {
		struct link *l = &links.data[i];
		ok = validate_link(t, l->id, l->name, err) &&
		     (cstr_eq(l->state, "deleting")
			      ? turso_delete_user(t, l->id, err)
			      : provision(t, l->id, l->name, cstr_eq(l->state, "provisioning"),
					  err));
	}
	for (size_t i = 0; i < links.len; i++) {
		xfree(links.data[i].name);
		xfree(links.data[i].state);
	}
	vec_free(&links);
	if (!ok) {
		goto fail;
	}
	*out = t;
	return true;
fail:
	turso_close(t);
	return false;
}

void turso_close(struct turso *t) {
	if (t == nullptr) {
		return;
	}
	for (size_t i = 0; i < t->users.len; i++) {
		tenant_free(t->users.data[i]);
	}
	vec_free(&t->users);
	vec_free(&t->routes);
	if (t->controller.hrana != nullptr) {
		db_conn_finish(&t->controller);
	}
	hrana_shared_unref(t->shared);
	pthread_mutex_destroy(&t->mu);
	xfree(t->cfg.organization);
	xfree(t->cfg.group);
	xfree(t->cfg.controller);
	explicit_bzero(t->cfg.group_token, strlen(t->cfg.group_token));
	explicit_bzero(t->cfg.api_token, strlen(t->cfg.api_token));
	xfree(t->cfg.group_token);
	xfree(t->cfg.api_token);
	xfree(t->api_base);
	xfree(t->sql_base);
	xfree(t);
}

bool turso_ready(struct turso *t, int64_t id, bool *ready, struct err **err) {
	struct db_stmt *st = db_prepare(
		&t->controller, "SELECT state = 'ready' FROM UserDatabase WHERE user = ?", err);
	if (st == nullptr) {
		return false;
	}
	bind_pos_i64(st, 1, id);
	int rc = db_step(st);
	bool ok = rc == SQLITE_ROW;
	if (ok) {
		*ready = col_bool(st, 0);
	} else if (rc == SQLITE_DONE) {
		err_set(err, 0, "Query returned no rows");
	} else {
		db_sql_err(&t->controller, err);
	}
	db_finalize(st);
	return ok;
}

bool turso_create_user(struct turso *t, struct db_user *u, struct err **err) {
	AUTOFREE char *name = turso_database_name();
	struct db_conn *c = &t->controller;
	if (!db_begin(c, err)) {
		return false;
	}
	struct db_stmt *st = db_prepare(
		c,
		"INSERT INTO User(username, password, admin, nick, realname, created_at, enabled, "
		"downstream_interacted_at, max_networks) VALUES (:username, :password, :admin, "
		":nick, :realname, :now, :enabled, :interacted, :max_networks)",
		err);
	if (st == nullptr) {
		db_rollback(c);
		return false;
	}
	AUTOFREE char *now = format_server_time_cstr(timestamp_now());
	bind_text(st, ":username", u->username);
	bind_text_or_null(st, ":password", u->password);
	bind_i64(st, ":admin", u->admin);
	bind_text_or_null(st, ":nick", u->nick);
	bind_text_or_null(st, ":realname", u->realname);
	bind_text(st, ":now", now);
	bind_i64(st, ":enabled", u->enabled);
	bind_time(st, ":interacted", &u->downstream_interacted_at);
	bind_i64(st, ":max_networks", u->max_networks);
	bool ok = db_step_done(st, err);
	db_finalize(st);
	int64_t id = db_last_insert_rowid(c);
	if (ok) {
		st = db_prepare(c,
				"INSERT INTO UserDatabase(user, name, state) VALUES (?, ?, "
				"'provisioning')",
				err);
		ok = st != nullptr;
		if (ok) {
			bind_pos_i64(st, 1, id);
			bind_pos_text(st, 2, name);
			ok = db_step_done(st, err);
			db_finalize(st);
		}
	}
	if (!ok || !db_commit(c, err)) {
		db_rollback(c);
		return false;
	}
	u->id = id;
	struct err *e = nullptr;
	if (!provision(t, id, name, true, &e)) {
		err_set(err, 0, "user reserved; restart soju to resume database provisioning: %s",
			err_msg(e));
		err_free(e);
		return false;
	}
	return true;
}

struct db_conn *turso_user(struct turso *t, int64_t id, struct err **err) {
	pthread_mutex_lock(&t->mu);
	struct db_conn *c = nullptr;
	for (size_t i = 0; i < t->users.len; i++) {
		if (t->users.data[i]->user == id) {
			c = &t->users.data[i]->conn;
		}
	}
	pthread_mutex_unlock(&t->mu);
	if (c == nullptr) {
		err_set(err, 0, "unknown Turso user %lld", (long long)id);
	}
	return c;
}

struct hrana *turso_fork_user(struct turso *t, int64_t id, struct err **err) {
	pthread_mutex_lock(&t->mu);
	struct hrana *h = nullptr;
	for (size_t i = 0; i < t->users.len; i++) {
		if (t->users.data[i]->user == id) {
			h = hrana_fork(t->users.data[i]->conn.hrana);
		}
	}
	pthread_mutex_unlock(&t->mu);
	if (h == nullptr) {
		err_set(err, 0, "unknown Turso user %lld", (long long)id);
	}
	return h;
}

bool turso_remember_network(struct turso *t, int64_t user, int64_t network, struct err **err) {
	pthread_mutex_lock(&t->mu);
	for (size_t i = 0; i < t->routes.len; i++) {
		struct turso_route *r = &t->routes.data[i];
		if (cstr_eq(r->table, "Network") && r->id == network) {
			bool same = r->user == user;
			pthread_mutex_unlock(&t->mu);
			return same ||
			       err_set(err, 0, "duplicate Network ID across Turso user databases");
		}
	}
	vec_push(&t->routes, ((struct turso_route){"Network", network, user}));
	pthread_mutex_unlock(&t->mu);
	return true;
}

// IDs of networks, channels and subscriptions are random 63-bit row IDs in
// Turso: ownership is cached, and found in the tenants after a restart
struct db_conn *turso_route(struct turso *t, const char *table, int64_t id, struct err **err) {
	pthread_mutex_lock(&t->mu);
	int64_t user = 0;
	for (size_t i = 0; i < t->routes.len; i++) {
		if (cstr_eq(t->routes.data[i].table, table) && t->routes.data[i].id == id) {
			user = t->routes.data[i].user;
		}
	}
	pthread_mutex_unlock(&t->mu);
	if (user != 0) {
		return turso_user(t, user, err);
	}
	AUTOFREE char *sql = xasprintf("SELECT id FROM %s WHERE id = ?", table);
	struct tenant *found = nullptr;
	for (size_t i = 0; i < t->users.len; i++) {
		struct tenant *u = t->users.data[i];
		struct db_stmt *st = db_prepare(&u->conn, sql, err);
		if (st == nullptr) {
			return nullptr;
		}
		bind_pos_i64(st, 1, id);
		int rc = db_step(st);
		db_finalize(st);
		if (rc != SQLITE_ROW && rc != SQLITE_DONE) {
			db_sql_err(&u->conn, err);
			return nullptr;
		}
		if (rc == SQLITE_ROW) {
			if (found != nullptr) {
				err_set(err, 0, "duplicate %s ID across Turso user databases",
					table);
				return nullptr;
			}
			found = u;
		}
	}
	if (found == nullptr) {
		err_set(err, 0, "unknown %s ID %lld", table, (long long)id);
		return nullptr;
	}
	pthread_mutex_lock(&t->mu);
	vec_push(&t->routes, ((struct turso_route){table, id, found->user}));
	pthread_mutex_unlock(&t->mu);
	return &found->conn;
}

static bool delete_database(struct turso *t, const char *name, struct err **err) {
	// A broad Platform API token must never delete a database that has
	// since moved outside the configured group
	AUTOFREE char *url = xasprintf("%s/organizations/%s/databases/%s", t->api_base,
				       t->cfg.organization, name);
	long status = 0;
	yyjson_doc *doc = nullptr;
	if (!request(t, "GET", url, t->cfg.api_token, nullptr, &status, &doc, err)) {
		return false;
	}
	if (status >= 200 && status <= 299) {
		const char *group = yyjson_get_str(yyjson_obj_get(
			yyjson_obj_get(yyjson_doc_get_root(doc), "database"), "group"));
		bool same = group != nullptr && cstr_eq(group, t->cfg.group);
		yyjson_doc_free(doc);
		if (!same) {
			return err_set(
				err, 0,
				"refusing to delete Turso database outside the configured group");
		}
	} else if (status != 404) {
		AUTO_STRBUF struct strbuf st = STRBUF_INIT;
		status_text(&st, status);
		return err_set(err, 0, "retrieve Turso database before deletion: HTTP %s",
			       strbuf_cstr(&st));
	}
	doc = nullptr;
	if (!request(t, "DELETE", url, t->cfg.api_token, nullptr, &status, &doc, err)) {
		return false;
	}
	yyjson_doc_free(doc);
	if ((status < 200 || status > 299) && status != 404) {
		AUTO_STRBUF struct strbuf st = STRBUF_INIT;
		status_text(&st, status);
		return err_set(err, 0, "delete Turso user database: HTTP %s", strbuf_cstr(&st));
	}
	return true;
}

bool turso_delete_user(struct turso *t, int64_t id, struct err **err) {
	struct db_conn *c = &t->controller;
	struct db_stmt *st = db_prepare(c, "SELECT name FROM UserDatabase WHERE user = ?", err);
	if (st == nullptr) {
		return false;
	}
	bind_pos_i64(st, 1, id);
	int rc = db_step(st);
	AUTOFREE char *name = rc == SQLITE_ROW ? col_lossy(st, 0) : nullptr;
	db_finalize(st);
	if (name == nullptr) {
		return rc == SQLITE_DONE ? err_set(err, 0, "Query returned no rows")
					 : db_sql_err(c, err);
	}
	// Persist the tombstone before deleting anything. Startup completes a
	// failed deletion instead of recreating the user's database.
	if (!validate_link(t, id, name, err) ||
	    !db_exec_i64(c,
			 "UPDATE UserDatabase SET state = 'deleting' WHERE user = ? AND "
			 "state!='deleting'",
			 id, err) ||
	    !delete_database(t, name, err)) {
		return false;
	}
	if (!db_begin(c, err)) {
		return false;
	}
	if (!db_exec_i64(c, "DELETE FROM DeviceCertificate WHERE user = ?", id, err) ||
	    !db_exec_i64(c, "DELETE FROM UserDatabase WHERE user = ?", id, err) ||
	    !db_exec_i64(c, "DELETE FROM User WHERE id = ?", id, err) || !db_commit(c, err)) {
		db_rollback(c);
		return false;
	}
	pthread_mutex_lock(&t->mu);
	for (size_t i = 0; i < t->users.len; i++) {
		if (t->users.data[i]->user == id) {
			tenant_free(t->users.data[i]);
			t->users.data[i] = t->users.data[--t->users.len];
			break;
		}
	}
	for (size_t i = 0; i < t->routes.len;) {
		if (t->routes.data[i].user == id) {
			t->routes.data[i] = t->routes.data[--t->routes.len];
		} else {
			i++;
		}
	}
	pthread_mutex_unlock(&t->mu);
	return true;
}
