#include "db/conn.h"

#include <stdio.h>
#include <openssl/rand.h>
#include <stdlib.h>

#include "base/alloc.h"
#include "base/utf8.h"
#include "db/hrana.h"

void db_value_free(struct db_value *v) {
	strbuf_free(&v->b);
	*v = (struct db_value){.type = DB_NULL};
}

void db_conn_init_local(struct db_conn *c, sqlite3 *handle) {
	*c = (struct db_conn){.sqlite = handle};
}

void db_conn_init_remote(struct db_conn *c, struct hrana *h) {
	*c = (struct db_conn){.remote = true, .hrana = h};
}

void db_conn_finish(struct db_conn *c) {
	db_finalize(c->insert_target);
	db_finalize(c->insert_message);
	if (c->remote) {
		hrana_free(c->hrana);
	} else {
		int rc = sqlite3_close(c->sqlite);
		if (rc != SQLITE_OK) {
			// An unfinalized statement: a bug (PLAN.md 2.4)
			fprintf(stderr, "soju: sqlite3_close: %s\n", sqlite3_errstr(rc));
			abort();
		}
	}
	xfree(c->err_msg);
	*c = (struct db_conn){0};
}

static void set_error(struct db_conn *c, int code, const char *msg) {
	c->err_code = code;
	xfree(c->err_msg);
	c->err_msg = xstrdup(msg);
}

bool db_sql_err(struct db_conn *c, struct err **err) {
	if (c->remote) {
		return err_set(err, c->err_code, "%s", c->err_msg != nullptr ? c->err_msg : "");
	}
	return err_set(err, sqlite3_extended_errcode(c->sqlite), "%s", sqlite3_errmsg(c->sqlite));
}

int db_errcode(struct db_conn *c) {
	return c->remote ? c->err_code : sqlite3_extended_errcode(c->sqlite);
}

struct db_stmt *db_prepare(struct db_conn *c, const char *sql, struct err **err) {
	if (c->remote) {
		struct db_stmt *st = xcalloc(1, sizeof(*st));
		st->conn = c;
		st->sql = xstrdup(sql);
		return st;
	}
	sqlite3_stmt *local = nullptr;
	if (sqlite3_prepare_v2(c->sqlite, sql, -1, &local, nullptr) != SQLITE_OK) {
		db_sql_err(c, err);
		sqlite3_finalize(local);
		return nullptr;
	}
	struct db_stmt *st = xcalloc(1, sizeof(*st));
	st->conn = c;
	st->local = local;
	return st;
}

static void free_rows(struct db_stmt *st) {
	for (size_t i = 0; i < st->rows.len; i++) {
		for (size_t j = 0; j < st->rows.data[i].len; j++) {
			db_value_free(&st->rows.data[i].data[j]);
		}
		vec_free(&st->rows.data[i]);
	}
	vec_free(&st->rows);
	strvec_free(&st->columns);
	st->next_row = 0;
	st->executed = false;
}

static void free_binds(struct db_stmt *st) {
	for (size_t i = 0; i < st->binds.len; i++) {
		xfree(st->binds.data[i].name);
		db_value_free(&st->binds.data[i].v);
	}
	vec_free(&st->binds);
}

int db_step(struct db_stmt *st) {
	if (!st->conn->remote) {
		return sqlite3_step(st->local);
	}
	struct db_conn *c = st->conn;
	if (!st->executed) {
		st->executed = true;
		struct err *e = nullptr;
		if (!hrana_execute(c->hrana, st->sql, st->binds.data, st->binds.len, &st->columns,
				   &st->rows, &c->last_id, &c->changes, &e)) {
			set_error(c, e->code != 0 ? e->code : SQLITE_ERROR, err_msg(e));
			err_free(e);
			return SQLITE_ERROR;
		}
	}
	if (st->next_row < st->rows.len) {
		st->next_row++;
		return SQLITE_ROW;
	}
	return SQLITE_DONE;
}

void db_reset(struct db_stmt *st) {
	if (st->conn->remote) {
		free_rows(st);
	} else {
		sqlite3_reset(st->local);
	}
}

void db_clear_bindings(struct db_stmt *st) {
	if (st->conn->remote) {
		free_binds(st);
	} else {
		sqlite3_clear_bindings(st->local);
	}
}

void db_finalize(struct db_stmt *st) {
	if (st == nullptr) {
		return;
	}
	sqlite3_finalize(st->local);
	free_rows(st);
	free_binds(st);
	xfree(st->sql);
	xfree(st);
}

int64_t db_last_insert_rowid(struct db_conn *c) {
	return c->remote ? c->last_id : sqlite3_last_insert_rowid(c->sqlite);
}

int64_t db_new_row_id(struct db_conn *c) {
	if (!c->remote) {
		return 0;
	}
	uint64_t r = 0;
	if (RAND_bytes((unsigned char *)&r, sizeof(r)) != 1) {
		abort();
	}
	int64_t id = (int64_t)(r & (uint64_t)INT64_MAX);
	return id > 0 ? id : 1;
}

int64_t db_changes(struct db_conn *c) {
	return c->remote ? c->changes : sqlite3_changes64(c->sqlite);
}

bool db_exec(struct db_conn *c, const char *sql, struct err **err) {
	if (c->remote) {
		struct err *e = nullptr;
		if (!hrana_sequence(c->hrana, sql, &e)) {
			set_error(c, e->code != 0 ? e->code : SQLITE_ERROR, err_msg(e));
			if (err != nullptr) {
				*err = e;
			} else {
				err_free(e);
			}
			return false;
		}
		return true;
	}
	char *msg = nullptr;
	int rc = sqlite3_exec(c->sqlite, sql, nullptr, nullptr, &msg);
	if (rc != SQLITE_OK) {
		err_set(err, sqlite3_extended_errcode(c->sqlite), "%s",
			msg != nullptr ? msg : sqlite3_errstr(rc));
		sqlite3_free(msg);
		return false;
	}
	return true;
}

bool db_step_done(struct db_stmt *st, struct err **err) {
	int rc;
	while ((rc = db_step(st)) == SQLITE_ROW) {
	}
	if (rc != SQLITE_DONE) {
		db_sql_err(st->conn, err);
		db_reset(st);
		return false;
	}
	db_reset(st);
	return true;
}

bool db_exec_one(struct db_conn *c, const char *sql, struct err **err) {
	struct db_stmt *st = db_prepare(c, sql, err);
	if (st == nullptr) {
		return false;
	}
	bool ok = db_step_done(st, err);
	db_finalize(st);
	return ok;
}

bool db_exec_i64(struct db_conn *c, const char *sql, int64_t v, struct err **err) {
	struct db_stmt *st = db_prepare(c, sql, err);
	if (st == nullptr) {
		return false;
	}
	bind_pos_i64(st, 1, v);
	bool ok = db_step_done(st, err);
	db_finalize(st);
	return ok;
}

bool db_query_i64(struct db_conn *c, const char *sql, int64_t *out, struct err **err) {
	struct db_stmt *st = db_prepare(c, sql, err);
	if (st == nullptr) {
		return false;
	}
	int rc = db_step(st);
	bool ok = rc == SQLITE_ROW;
	if (ok) {
		*out = col_i64(st, 0);
	} else if (rc == SQLITE_DONE) {
		err_set(err, 0, "Query returned no rows");
	} else {
		db_sql_err(c, err);
	}
	db_finalize(st);
	return ok;
}

bool db_begin(struct db_conn *c, struct err **err) {
	if (c->remote) {
		struct err *e = nullptr;
		if (!hrana_begin(c->hrana, &e)) {
			set_error(c, e->code != 0 ? e->code : SQLITE_ERROR, err_msg(e));
			if (err != nullptr) {
				*err = e;
			} else {
				err_free(e);
			}
			return false;
		}
		return true;
	}
	return db_exec(c, "BEGIN IMMEDIATE", err);
}

bool db_commit(struct db_conn *c, struct err **err) {
	if (c->remote) {
		struct err *e = nullptr;
		if (!hrana_commit(c->hrana, &e)) {
			set_error(c, e->code != 0 ? e->code : SQLITE_ERROR, err_msg(e));
			if (err != nullptr) {
				*err = e;
			} else {
				err_free(e);
			}
			return false;
		}
		return true;
	}
	return db_exec(c, "COMMIT", err);
}

void db_rollback(struct db_conn *c) {
	if (c->remote) {
		hrana_rollback(c->hrana);
	} else if (!sqlite3_get_autocommit(c->sqlite)) {
		(void)db_exec(c, "ROLLBACK", nullptr);
	}
}

// Binding

static int local_param(struct db_stmt *st, const char *name) {
	int i = sqlite3_bind_parameter_index(st->local, name);
	if (i == 0) {
		fprintf(stderr, "soju: SQL parameter %s missing in %s\n", name,
			sqlite3_sql(st->local));
		abort();
	}
	return i;
}

// The remote binding of a name or index, replacing an earlier one
static struct db_value *remote_bind(struct db_stmt *st, const char *name, int index) {
	for (size_t i = 0; i < st->binds.len; i++) {
		struct db_bind *b = &st->binds.data[i];
		if (name != nullptr ? (b->name != nullptr && cstr_eq(b->name, name))
				    : (b->name == nullptr && b->index == index)) {
			db_value_free(&b->v);
			return &b->v;
		}
	}
	if (name != nullptr && strstr(st->sql, name) == nullptr) {
		fprintf(stderr, "soju: SQL parameter %s missing in %s\n", name, st->sql);
		abort();
	}
	vec_push(&st->binds, ((struct db_bind){.name = name != nullptr ? xstrdup(name) : nullptr,
					       .index = index}));
	return &st->binds.data[st->binds.len - 1].v;
}

static void set_i64(struct db_stmt *st, const char *name, int index, int64_t v) {
	if (st->conn->remote) {
		*remote_bind(st, name, index) = (struct db_value){.type = DB_INTEGER, .i = v};
	} else {
		sqlite3_bind_int64(st->local, name != nullptr ? local_param(st, name) : index, v);
	}
}

static void set_bytes(struct db_stmt *st, const char *name, int index, enum db_value_type type,
		      const void *p, size_t len) {
	if (st->conn->remote) {
		struct db_value *v = remote_bind(st, name, index);
		v->type = type;
		strbuf_append(&v->b, len > 0 ? p : "", len);
		return;
	}
	int i = name != nullptr ? local_param(st, name) : index;
	if (type == DB_TEXT) {
		sqlite3_bind_text64(st->local, i, len > 0 ? p : "", len, SQLITE_TRANSIENT,
				    SQLITE_UTF8);
	} else {
		sqlite3_bind_blob64(st->local, i, len > 0 ? p : "", len, SQLITE_TRANSIENT);
	}
}

static void set_null(struct db_stmt *st, const char *name, int index) {
	if (st->conn->remote) {
		*remote_bind(st, name, index) = (struct db_value){.type = DB_NULL};
	} else {
		sqlite3_bind_null(st->local, name != nullptr ? local_param(st, name) : index);
	}
}

void bind_i64(struct db_stmt *st, const char *name, int64_t v) {
	set_i64(st, name, 0, v);
}

void bind_text(struct db_stmt *st, const char *name, const char *s) {
	set_bytes(st, name, 0, DB_TEXT, s, strlen(s));
}

void bind_text_or_null(struct db_stmt *st, const char *name, const char *s) {
	if (s[0] == '\0') {
		bind_null(st, name);
	} else {
		bind_text(st, name, s);
	}
}

void bind_null(struct db_stmt *st, const char *name) {
	set_null(st, name, 0);
}

void bind_blob(struct db_stmt *st, const char *name, const struct db_blob *b) {
	if (!b->set) {
		bind_null(st, name);
		return;
	}
	set_bytes(st, name, 0, DB_BLOB, b->data.buf, b->data.len);
}

void bind_bytes(struct db_stmt *st, const char *name, const void *p, size_t len) {
	set_bytes(st, name, 0, DB_BLOB, p, len);
}

void bind_time(struct db_stmt *st, const char *name, const struct db_time *t) {
	if (!t->set) {
		bind_null(st, name);
		return;
	}
	AUTOFREE char *s = format_server_time_cstr(t->t);
	bind_text(st, name, s);
}

void bind_pos_i64(struct db_stmt *st, int index, int64_t v) {
	set_i64(st, nullptr, index, v);
}

void bind_pos_text(struct db_stmt *st, int index, const char *s) {
	set_bytes(st, nullptr, index, DB_TEXT, s, strlen(s));
}

void bind_pos_null(struct db_stmt *st, int index) {
	set_null(st, nullptr, index);
}

void bind_pos_blob(struct db_stmt *st, int index, const void *p, size_t len) {
	set_bytes(st, nullptr, index, DB_BLOB, p, len);
}

// Columns

static const struct db_value *remote_col(struct db_stmt *st, int i) {
	static const struct db_value null = {.type = DB_NULL};
	if (st->next_row == 0 || st->next_row > st->rows.len) {
		return &null;
	}
	const db_row *row = &st->rows.data[st->next_row - 1];
	return i >= 0 && (size_t)i < row->len ? &row->data[i] : &null;
}

int col_type(struct db_stmt *st, int i) {
	return st->conn->remote ? (int)remote_col(st, i)->type : sqlite3_column_type(st->local, i);
}

struct str col_bytes(struct db_stmt *st, int i) {
	if (st->conn->remote) {
		const struct db_value *v = remote_col(st, i);
		return v->type == DB_TEXT || v->type == DB_BLOB ? strbuf_str(&v->b) : STR_EMPTY;
	}
	int type = sqlite3_column_type(st->local, i);
	const void *p = type == SQLITE_BLOB ? sqlite3_column_blob(st->local, i)
					    : (const void *)sqlite3_column_text(st->local, i);
	return str_new(p != nullptr ? p : "", (size_t)sqlite3_column_bytes(st->local, i));
}

static const char *column_name(struct db_stmt *st, int i) {
	if (st->conn->remote) {
		return i >= 0 && (size_t)i < st->columns.len ? st->columns.data[i] : "";
	}
	const char *name = sqlite3_column_name(st->local, i);
	return name != nullptr ? name : "";
}

char *col_lossy(struct db_stmt *st, int i) {
	if (col_type(st, i) == SQLITE_NULL) {
		return xstrdup("");
	}
	struct strbuf sb = STRBUF_INIT;
	utf8_lossy(&sb, col_bytes(st, i));
	return strbuf_take(&sb);
}

static const char *type_name(int type) {
	switch (type) {
	case SQLITE_INTEGER:
		return "Integer";
	case SQLITE_FLOAT:
		return "Real";
	case SQLITE_BLOB:
		return "Blob";
	case SQLITE_NULL:
		return "Null";
	default:
		return "Text";
	}
}

bool col_text(struct db_stmt *st, int i, char **out, struct err **err) {
	int type = col_type(st, i);
	if (type != SQLITE_TEXT) {
		// rusqlite: InvalidColumnType
		err_set(err, 0, "Invalid column type %s at index: %d, name: %s", type_name(type), i,
			column_name(st, i));
		return false;
	}
	struct str s = col_bytes(st, i);
	if (!utf8_valid(s)) {
		return err_set(err, 0,
			       "Conversion error from type Text at index: %d, invalid utf-8", i);
	}
	*out = str_dup(s);
	return true;
}

bool col_text_opt(struct db_stmt *st, int i, char **out, struct err **err) {
	if (col_type(st, i) == SQLITE_NULL) {
		*out = xstrdup("");
		return true;
	}
	return col_text(st, i, out, err);
}

int64_t col_i64(struct db_stmt *st, int i) {
	if (!st->conn->remote) {
		return sqlite3_column_int64(st->local, i);
	}
	const struct db_value *v = remote_col(st, i);
	switch (v->type) {
	case DB_INTEGER:
		return v->i;
	case DB_FLOAT:
		return (int64_t)v->f;
	case DB_TEXT: {
		long long n = 0;
		return str_parse_i64(strbuf_str(&v->b), &n) ? n : 0;
	}
	default:
		return 0;
	}
}

bool col_bool(struct db_stmt *st, int i) {
	return col_i64(st, i) != 0;
}

bool col_time(struct db_stmt *st, int i, struct db_time *out, struct err **err) {
	*out = (struct db_time){0};
	if (col_type(st, i) == SQLITE_NULL) {
		return true;
	}
	AUTOFREE char *s = nullptr;
	if (!col_text(st, i, &s, err) || s == nullptr) {
		return false;
	}
	if (!parse_server_time(str_from(s), &out->t)) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(s));
		return err_set(err, 0, "cannot parse time %s", q);
	}
	out->set = true;
	return true;
}

void col_blob(struct db_stmt *st, int i, struct db_blob *out) {
	*out = (struct db_blob){0};
	if (col_type(st, i) == SQLITE_NULL) {
		return;
	}
	out->set = true;
	strbuf_append_str(&out->data, col_bytes(st, i));
	strbuf_append(&out->data, "", 0);
}
