#include "turso_mock.h"

#include <arpa/inet.h>
#include <ctype.h>
#include <poll.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <unistd.h>
#include <yyjson.h>

#include "base/alloc.h"
#include "base/base64.h"
#include "testsupport.h"

struct mock_stream {
	char *db;
	sqlite3 *conn;
};

static void stream_free(void *p) {
	struct mock_stream *s = p;
	if (s != nullptr) {
		sqlite3_close_v2(s->conn);
		xfree(s->db);
		xfree(s);
	}
}

// Whether a pipeline body has SQL containing needle
static bool contains_sql(yyjson_val *body, const char *needle) {
	if (needle == nullptr) {
		return false;
	}
	size_t i, n;
	yyjson_val *r;
	yyjson_arr_foreach(yyjson_obj_get(body, "requests"), i, n, r) {
		const char *sql = yyjson_get_str(yyjson_obj_get(r, "sql"));
		if (sql == nullptr) {
			sql = yyjson_get_str(yyjson_obj_get(yyjson_obj_get(r, "stmt"), "sql"));
		}
		if (sql != nullptr && strstr(sql, needle) != nullptr) {
			return true;
		}
		size_t j, m;
		yyjson_val *step;
		yyjson_arr_foreach(yyjson_obj_get(yyjson_obj_get(r, "batch"), "steps"), j, m,
				   step) {
			sql = yyjson_get_str(yyjson_obj_get(yyjson_obj_get(step, "stmt"), "sql"));
			if (sql != nullptr && strstr(sql, needle) != nullptr) {
				return true;
			}
		}
	}
	return false;
}

// SQLite accepts these statements, but the hosted SQL parser rejects them
static bool cloud_sql_ok(const char *sql) {
	AUTOFREE char *lower = xstrdup(sql);
	for (char *c = lower; *c != '\0'; c++) {
		*c = (char)tolower((unsigned char)*c);
	}
	struct str rest = str_from(lower), part;
	while (str_next_field(&rest, ';', &part)) {
		part = str_trim_ascii(part);
		static const char *const bad[] = {"vacuum", "analyze", "pragma wal_checkpoint",
						  "pragma optimize", "pragma shrink_memory"};
		for (size_t i = 0; i < sizeof(bad) / sizeof(bad[0]); i++) {
			if (str_starts_with(part, str_from(bad[i]))) {
				return false;
			}
		}
		if (str_starts_with(part, STR_LIT("pragma user_version")) &&
		    (str_index(part, '=') >= 0 || str_index(part, '(') >= 0)) {
			return false;
		}
	}
	return true;
}

static void bind_arg(sqlite3_stmt *st, int index, yyjson_val *arg) {
	const char *type = yyjson_get_str(yyjson_obj_get(arg, "type"));
	yyjson_val *value = yyjson_obj_get(arg, "value");
	assert_non_null(type);
	if (cstr_eq(type, "null")) {
		sqlite3_bind_null(st, index);
	} else if (cstr_eq(type, "integer")) {
		sqlite3_bind_int64(st, index, strtoll(yyjson_get_str(value), nullptr, 10));
	} else if (cstr_eq(type, "float")) {
		sqlite3_bind_double(st, index, yyjson_get_num(value));
	} else if (cstr_eq(type, "text")) {
		sqlite3_bind_text(st, index, yyjson_get_str(value), (int)yyjson_get_len(value),
				  SQLITE_TRANSIENT);
	} else if (cstr_eq(type, "blob")) {
		AUTO_STRBUF struct strbuf s = STRBUF_INIT, out = STRBUF_INIT;
		yyjson_val *b = yyjson_obj_get(arg, "base64");
		strbuf_append(&s, yyjson_get_str(b), yyjson_get_len(b));
		while (s.len > 0 && s.buf[s.len - 1] == '=') {
			strbuf_truncate(&s, s.len - 1);
		}
		while (s.len % 4 != 0) {
			strbuf_append_char(&s, '=');
		}
		assert_true(base64_decode(&out, BASE64_STANDARD, strbuf_str(&s), nullptr));
		sqlite3_bind_blob(st, index, out.len > 0 ? out.buf : "", (int)out.len,
				  SQLITE_TRANSIENT);
	} else {
		fail_msg("invalid protocol argument %s", type);
	}
}

static void error_result(yyjson_mut_doc *d, yyjson_mut_val *out, sqlite3 *conn, const char *msg) {
	yyjson_mut_obj_add_str(d, out, "type", "error");
	yyjson_mut_val *e = yyjson_mut_obj_add_obj(d, out, "error");
	bool unique = conn != nullptr && sqlite3_extended_errcode(conn) == SQLITE_CONSTRAINT_UNIQUE;
	yyjson_mut_obj_add_str(d, e, "code", unique ? "SQLITE_CONSTRAINT_UNIQUE" : "SQLITE_ERROR");
	yyjson_mut_obj_add_strcpy(d, e, "message", msg);
}

// Runs a statement: its result object, or nullptr with *msg set
static yyjson_mut_val *execute(yyjson_mut_doc *d, sqlite3 *conn, yyjson_val *stmt, char **msg) {
	const char *sql = yyjson_get_str(yyjson_obj_get(stmt, "sql"));
	if (!cloud_sql_ok(sql)) {
		*msg = xstrdup("unsupported cloud SQL statement");
		return nullptr;
	}
	sqlite3_stmt *st = nullptr;
	if (sqlite3_prepare_v2(conn, sql, -1, &st, nullptr) != SQLITE_OK) {
		*msg = xstrdup(sqlite3_errmsg(conn));
		return nullptr;
	}
	size_t i, n;
	yyjson_val *arg;
	yyjson_arr_foreach(yyjson_obj_get(stmt, "args"), i, n, arg) {
		bind_arg(st, (int)i + 1, arg);
	}
	yyjson_arr_foreach(yyjson_obj_get(stmt, "named_args"), i, n, arg) {
		const char *name = yyjson_get_str(yyjson_obj_get(arg, "name"));
		int index = sqlite3_bind_parameter_index(st, name);
		assert_true(index > 0);
		bind_arg(st, index, yyjson_obj_get(arg, "value"));
	}
	yyjson_mut_val *result = yyjson_mut_obj(d);
	yyjson_mut_val *cols = yyjson_mut_obj_add_arr(d, result, "cols");
	int count = sqlite3_column_count(st);
	for (int c = 0; c < count; c++) {
		yyjson_mut_val *col = yyjson_mut_arr_add_obj(d, cols);
		yyjson_mut_obj_add_strcpy(d, col, "name", sqlite3_column_name(st, c));
		yyjson_mut_obj_add_null(d, col, "decltype");
	}
	yyjson_mut_val *rows = yyjson_mut_obj_add_arr(d, result, "rows");
	int rc;
	while ((rc = sqlite3_step(st)) == SQLITE_ROW) {
		yyjson_mut_val *row = yyjson_mut_arr_add_arr(d, rows);
		for (int c = 0; c < count; c++) {
			yyjson_mut_val *v = yyjson_mut_arr_add_obj(d, row);
			switch (sqlite3_column_type(st, c)) {
			case SQLITE_NULL:
				yyjson_mut_obj_add_str(d, v, "type", "null");
				break;
			case SQLITE_INTEGER: {
				char num[32];
				snprintf(num, sizeof(num), "%lld", sqlite3_column_int64(st, c));
				yyjson_mut_obj_add_str(d, v, "type", "integer");
				yyjson_mut_obj_add_strcpy(d, v, "value", num);
				break;
			}
			case SQLITE_FLOAT:
				yyjson_mut_obj_add_str(d, v, "type", "float");
				yyjson_mut_obj_add_real(d, v, "value",
							sqlite3_column_double(st, c));
				break;
			case SQLITE_TEXT:
				yyjson_mut_obj_add_str(d, v, "type", "text");
				yyjson_mut_obj_add_strncpy(d, v, "value",
							   (const char *)sqlite3_column_text(st, c),
							   (size_t)sqlite3_column_bytes(st, c));
				break;
			default: {
				AUTO_STRBUF struct strbuf b64 = STRBUF_INIT;
				const void *p = sqlite3_column_blob(st, c);
				base64_encode(&b64, BASE64_STANDARD, p != nullptr ? p : "",
					      (size_t)sqlite3_column_bytes(st, c));
				while (b64.len > 0 && b64.buf[b64.len - 1] == '=') {
					strbuf_truncate(&b64, b64.len - 1);
				}
				yyjson_mut_obj_add_str(d, v, "type", "blob");
				yyjson_mut_obj_add_strncpy(
					d, v, "base64", b64.buf != nullptr ? b64.buf : "", b64.len);
			}
			}
		}
	}
	if (rc != SQLITE_DONE) {
		*msg = xstrdup(sqlite3_errmsg(conn));
		sqlite3_finalize(st);
		return nullptr;
	}
	sqlite3_finalize(st);
	yyjson_mut_obj_add_int(d, result, "affected_row_count", sqlite3_changes64(conn));
	char id[32];
	snprintf(id, sizeof(id), "%lld", sqlite3_last_insert_rowid(conn));
	yyjson_mut_obj_add_strcpy(d, result, "last_insert_rowid", id);
	return result;
}

static size_t message_insert_lines(yyjson_val *stmt) {
	const char *sql = yyjson_get_str(yyjson_obj_get(stmt, "sql"));
	if (sql == nullptr || strncmp(sql, "INSERT INTO Message(", 20) != 0) {
		return 0;
	}
	yyjson_val *args = yyjson_obj_get(stmt, "args");
	return yyjson_is_arr(args) ? yyjson_arr_size(args) / 4 : 1;
}

static sqlite3 *open_db(struct turso_mock *m, const char *name) {
	AUTOFREE char *path = xasprintf("%s/%s", m->root, name);
	sqlite3 *conn = nullptr;
	assert_int_equal(sqlite3_open(path, &conn), SQLITE_OK);
	sqlite3_busy_timeout(conn, 5000);
	sqlite3_extended_result_codes(conn, 1);
	return conn;
}

static void batch(struct turso_mock *m, yyjson_mut_doc *d, yyjson_mut_val *out, sqlite3 *conn,
		  yyjson_val *steps, const char *index) {
	yyjson_mut_obj_add_str(d, out, "type", "ok");
	yyjson_mut_val *resp = yyjson_mut_obj_add_obj(d, out, "response");
	yyjson_mut_obj_add_str(d, resp, "type", "batch");
	yyjson_mut_val *result = yyjson_mut_obj_add_obj(d, resp, "result");
	yyjson_mut_val *results = yyjson_mut_obj_add_arr(d, result, "step_results");
	yyjson_mut_val *errors = yyjson_mut_obj_add_arr(d, result, "step_errors");
	yyjson_mut_obj_add_strcpy(d, result, "replication_index", index);
	size_t i, n, lines = 0;
	yyjson_val *step;
	yyjson_arr_foreach(steps, i, n, step) {
		lines += message_insert_lines(yyjson_obj_get(step, "stmt"));
	}
	if (m->st.max_batch_lines != 0 && lines > m->st.max_batch_lines) {
		yyjson_arr_foreach(steps, i, n, step) {
			yyjson_mut_arr_add_null(d, results);
			if (i == 0) {
				yyjson_mut_val *e = yyjson_mut_arr_add_obj(d, errors);
				yyjson_mut_obj_add_str(d, e, "code", "TRANSACTION_TIMEOUT");
				yyjson_mut_obj_add_str(d, e, "message", "mock processing budget");
			} else {
				yyjson_mut_arr_add_null(d, errors);
			}
		}
		return;
	}
	yyjson_arr_foreach(steps, i, n, step) {
		yyjson_val *cond = yyjson_obj_get(step, "condition");
		bool run = true;
		if (cond != nullptr && !yyjson_is_null(cond)) {
			assert_string_equal(yyjson_get_str(yyjson_obj_get(cond, "type")), "ok");
			size_t prev = yyjson_get_uint(yyjson_obj_get(cond, "step"));
			run = yyjson_mut_is_obj(yyjson_mut_arr_get(results, prev));
		}
		char *msg = nullptr;
		yyjson_mut_val *r =
			run ? execute(d, conn, yyjson_obj_get(step, "stmt"), &msg) : nullptr;
		if (r != nullptr) {
			yyjson_mut_arr_append(results, r);
			yyjson_mut_arr_add_null(d, errors);
		} else {
			yyjson_mut_arr_add_null(d, results);
			if (msg != nullptr) {
				yyjson_mut_val *e = yyjson_mut_arr_add_obj(d, errors);
				yyjson_mut_obj_add_str(d, e, "code", "SQLITE_ERROR");
				yyjson_mut_obj_add_strcpy(d, e, "message", msg);
			} else {
				yyjson_mut_arr_add_null(d, errors);
			}
		}
		xfree(msg);
	}
}

// Runs a pipeline on database name; the response object
static yyjson_mut_val *pipeline(struct turso_mock *m, yyjson_mut_doc *d, const char *name,
				yyjson_val *body) {
	assert_true(strmap_has(&m->st.databases, name));
	const char *baton = yyjson_get_str(yyjson_obj_get(body, "baton"));
	sqlite3 *conn;
	if (baton != nullptr) {
		struct mock_stream *s = strmap_take(&m->st.streams, baton);
		assert_non_null(s);
		assert_string_equal(s->db, name);
		conn = s->conn;
		s->conn = nullptr;
		stream_free(s);
	} else {
		conn = open_db(m, name);
	}
	bool closed = false;
	char index[32];
	snprintf(index, sizeof(index), "%llu", (unsigned long long)++m->st.replication_index);
	yyjson_mut_val *resp = yyjson_mut_obj(d);
	yyjson_mut_val *results = yyjson_mut_arr(d);
	size_t i, n;
	yyjson_val *r;
	yyjson_arr_foreach(yyjson_obj_get(body, "requests"), i, n, r) {
		const char *type = yyjson_get_str(yyjson_obj_get(r, "type"));
		yyjson_mut_val *out = yyjson_mut_arr_add_obj(d, results);
		if (cstr_eq(type, "batch")) {
			batch(m, d, out, conn, yyjson_obj_get(yyjson_obj_get(r, "batch"), "steps"),
			      index);
			continue;
		}
		yyjson_mut_val *response = yyjson_mut_obj(d);
		char *msg = nullptr;
		if (cstr_eq(type, "execute")) {
			yyjson_mut_val *result = execute(d, conn, yyjson_obj_get(r, "stmt"), &msg);
			if (result != nullptr) {
				yyjson_mut_obj_add_strcpy(d, result, "replication_index", index);
				yyjson_mut_obj_add_str(d, response, "type", "execute");
				yyjson_mut_obj_add_val(d, response, "result", result);
			}
		} else if (cstr_eq(type, "sequence")) {
			const char *sql = yyjson_get_str(yyjson_obj_get(r, "sql"));
			char *e = nullptr;
			if (!cloud_sql_ok(sql)) {
				msg = xstrdup("unsupported cloud SQL statement");
			} else if (sqlite3_exec(conn, sql, nullptr, nullptr, &e) != SQLITE_OK) {
				msg = xstrdup(e != nullptr ? e : "error");
			} else {
				yyjson_mut_obj_add_str(d, response, "type", "sequence");
			}
			sqlite3_free(e);
		} else if (cstr_eq(type, "close")) {
			assert_false(closed);
			closed = true;
			yyjson_mut_obj_add_str(d, response, "type", "close");
		} else {
			fail_msg("unsupported mock operation %s", type);
		}
		if (msg != nullptr) {
			error_result(d, out, conn, msg);
			xfree(msg);
		} else {
			yyjson_mut_obj_add_str(d, out, "type", "ok");
			yyjson_mut_obj_add_val(d, out, "response", response);
		}
	}
	// Hosted Turso may close a stateless stream after COMMIT without an
	// explicit close request. Legacy libSQL keeps returning a baton.
	if (closed || (m->st.close_idle_streams && sqlite3_get_autocommit(conn))) {
		sqlite3_close_v2(conn);
		yyjson_mut_obj_add_null(d, resp, "baton");
	} else {
		char b[32];
		snprintf(b, sizeof(b), "%llu", (unsigned long long)++m->st.next_stream);
		struct mock_stream *s = xcalloc(1, sizeof(*s));
		*s = (struct mock_stream){.db = xstrdup(name), .conn = conn};
		strmap_set(&m->st.streams, b, s);
		yyjson_mut_obj_add_strcpy(d, resp, "baton", b);
	}
	yyjson_mut_obj_add_null(d, resp, "base_url");
	yyjson_mut_obj_add_val(d, resp, "results", results);
	return resp;
}

// Platform API: the status and response object (or nullptr)
static int platform(struct turso_mock *m, yyjson_mut_doc *d, const char *method, const char *path,
		    yyjson_val *body, yyjson_mut_val **resp) {
	static const char collection[] = "/v1/organizations/example/databases";
	*resp = nullptr;
	if (cstr_eq(path, collection) && cstr_eq(method, "POST")) {
		if (m->st.fail_create) {
			return 500;
		}
		const char *name = yyjson_get_str(yyjson_obj_get(body, "name"));
		const char *group = yyjson_get_str(yyjson_obj_get(body, "group"));
		if (strmap_has(&m->st.databases, name)) {
			return 409;
		}
		strmap_set(&m->st.databases, name, xstrdup(group));
		*resp = yyjson_mut_obj(d);
		yyjson_mut_obj_add_strcpy(d, yyjson_mut_obj_add_obj(d, *resp, "database"), "Name",
					  name);
		return 200;
	}
	if (strncmp(path, collection, sizeof(collection) - 1) != 0 ||
	    path[sizeof(collection) - 1] != '/') {
		return 404;
	}
	const char *name = path + sizeof(collection);
	if (cstr_eq(method, "DELETE")) {
		if (m->st.fail_delete) {
			return 503;
		}
		char *group = strmap_take(&m->st.databases, name);
		if (group == nullptr) {
			return 404;
		}
		xfree(group);
		const char *b;
		void *v;
		for (size_t it = 0; strmap_next(&m->st.streams, &it, &b, &v);) {
			if (cstr_eq(((struct mock_stream *)v)->db, name)) {
				AUTOFREE char *key = xstrdup(b);
				stream_free(strmap_take(&m->st.streams, key));
				it = 0;
			}
		}
		AUTOFREE char *path_db = xasprintf("%s/%s", m->root, name);
		unlink(path_db);
		return 204;
	}
	const char *group = strmap_get(&m->st.databases, name);
	if (group == nullptr) {
		return 404;
	}
	*resp = yyjson_mut_obj(d);
	yyjson_mut_val *db = yyjson_mut_obj_add_obj(d, *resp, "database");
	yyjson_mut_obj_add_strcpy(d, db, "Name", name);
	AUTOFREE char *host = xasprintf("%s-example.turso.io", name);
	yyjson_mut_obj_add_strcpy(d, db, "Hostname", host);
	yyjson_mut_obj_add_strcpy(d, db, "group", m->st.bad_group ? "different-group" : group);
	return 200;
}

static bool read_request(int fd, struct strbuf *req, size_t *head_len, size_t *body_len) {
	for (;;) {
		ptrdiff_t end = str_find(strbuf_str(req), STR_LIT("\r\n\r\n"));
		if (end >= 0 && req->buf != nullptr) {
			const char *cl = strcasestr(req->buf, "content-length:");
			*head_len = (size_t)end + 4;
			*body_len = cl != nullptr && cl < req->buf + end
					    ? strtoul(cl + strlen("content-length:"), nullptr, 10)
					    : 0;
			if (req->len >= *head_len + *body_len) {
				return true;
			}
		}
		struct pollfd p = {.fd = fd, .events = POLLIN};
		if (poll(&p, 1, 5000) != 1) {
			return false;
		}
		char buf[16384];
		ssize_t n = recv(fd, buf, sizeof(buf), 0);
		if (n <= 0) {
			return false;
		}
		strbuf_append(req, buf, (size_t)n);
	}
}

struct handler {
	struct turso_mock *m;
	int fd;
};

static void *handle(void *arg) {
	struct handler *h = arg;
	struct turso_mock *m = h->m;
	int fd = h->fd;
	xfree(h);
	struct strbuf req = STRBUF_INIT;
	size_t head_len = 0, body_len = 0;
	if (!read_request(fd, &req, &head_len, &body_len)) {
		strbuf_free(&req);
		close(fd);
		return nullptr;
	}
	char method[16] = "", path[512] = "";
	if (sscanf(req.buf, "%15s %511s", method, path) != 2) {
		strbuf_free(&req);
		close(fd);
		return nullptr;
	}
	const char *auth = strcasestr(req.buf, "\r\nauthorization: ");
	AUTOFREE char *token = nullptr;
	if (auth != nullptr && auth < req.buf + head_len) {
		auth += strlen("\r\nauthorization: ");
		token = xstrndup(auth, (size_t)(strstr(auth, "\r\n") - auth));
	}
	yyjson_doc *doc = body_len > 0 ? yyjson_read(req.buf + head_len, body_len, 0) : nullptr;
	yyjson_val *body = yyjson_doc_get_root(doc);
	yyjson_mut_doc *d = yyjson_mut_doc_new(nullptr);
	yyjson_mut_val *resp = nullptr;
	int status = 200;
	unsigned delay = 0;
	enum turso_fault fault = TURSO_FAULT_NONE;

	pthread_mutex_lock(&m->mu);
	if (contains_sql(body, m->st.delay_sql)) {
		delay = m->st.delay_ms;
	}
	if (contains_sql(body, m->st.delay_before_sql)) {
		unsigned ms = m->st.delay_before_ms;
		xfree(m->st.delay_before_sql);
		m->st.delay_before_sql = nullptr;
		pthread_mutex_unlock(&m->mu);
		usleep(ms * 1000);
		pthread_mutex_lock(&m->mu);
	}
	if (contains_sql(body, m->st.fault_sql)) {
		fault = m->st.fault;
		xfree(m->st.fault_sql);
		m->st.fault_sql = nullptr;
	}
	bool platform_path = strncmp(path, "/v1/", 4) == 0;
	const char *expected = platform_path ? "Bearer platform-secret" : "Bearer sql-secret";
	if (token == nullptr || !cstr_eq(token, expected)) {
		status = 401;
	} else if (platform_path) {
		status = platform(m, d, method, path, body, &resp);
	} else {
		AUTOFREE char *name = xstrdup(path + strlen("/db/"));
		char *suffix = strstr(name, "/v2/pipeline");
		assert_non_null(suffix);
		*suffix = '\0';
		const char *baton = yyjson_get_str(yyjson_obj_get(body, "baton"));
		if (baton != nullptr && !strmap_has(&m->st.streams, baton)) {
			status = 400;
		} else if (m->st.fail_sql != nullptr && cstr_eq(m->st.fail_sql, name)) {
			status = 503;
		} else {
			vec_push(&m->st.request_sizes, body_len);
			strvec_push_take(&m->st.requests, xstrndup(req.buf + head_len, body_len));
			if (m->st.max_body != 0 && body_len > m->st.max_body) {
				status = 413;
			} else {
				resp = pipeline(m, d, name, body);
			}
		}
	}
	if (fault != TURSO_FAULT_NONE && resp != nullptr) {
		yyjson_mut_val *results = yyjson_mut_obj_get(resp, "results");
		switch (fault) {
		case TURSO_FAULT_LOST_STREAM:
		case TURSO_FAULT_HTTP_AFTER:
		case TURSO_FAULT_EMPTY_BODY: {
			const char *b = yyjson_mut_get_str(yyjson_mut_obj_get(resp, "baton"));
			if (b != nullptr) {
				stream_free(strmap_take(&m->st.streams, b));
			}
			yyjson_mut_obj_put(resp, yyjson_mut_str(d, "baton"), yyjson_mut_null(d));
			if (fault == TURSO_FAULT_HTTP_AFTER) {
				status = 503;
			}
			break;
		}
		case TURSO_FAULT_MISSING_RESULTS:
			yyjson_mut_arr_remove_last(results);
			break;
		case TURSO_FAULT_WRONG_RESPONSE_TYPE: {
			yyjson_mut_val *r0 =
				yyjson_mut_obj_get(yyjson_mut_arr_get(results, 0), "response");
			yyjson_mut_obj_put(r0, yyjson_mut_str(d, "type"),
					   yyjson_mut_str(d, "unexpected"));
			break;
		}
		case TURSO_FAULT_NONE:
			break;
		}
	}
	pthread_mutex_unlock(&m->mu);
	if (delay > 0) {
		usleep(delay * 1000);
	}
	size_t len = 0;
	char *json = nullptr;
	if (resp != nullptr && fault != TURSO_FAULT_EMPTY_BODY && status != 204) {
		yyjson_mut_doc_set_root(d, resp);
		json = yyjson_mut_write(d, 0, &len);
	}
	AUTO_STRBUF struct strbuf out = STRBUF_INIT;
	strbuf_printf(&out,
		      "HTTP/1.1 %d Mock\r\nContent-Type: application/json\r\nContent-Length: "
		      "%zu\r\nConnection: close\r\n\r\n",
		      status, len);
	strbuf_append(&out, json != nullptr ? json : "", len);
	free(json);
	(void)!send(fd, out.buf, out.len, MSG_NOSIGNAL);
	yyjson_mut_doc_free(d);
	yyjson_doc_free(doc);
	strbuf_free(&req);
	close(fd);
	return nullptr;
}

static void *serve(void *arg) {
	struct turso_mock *m = arg;
	for (;;) {
		pthread_mutex_lock(&m->mu);
		bool stop = m->stop;
		pthread_mutex_unlock(&m->mu);
		if (stop) {
			break;
		}
		struct pollfd p = {.fd = m->listener, .events = POLLIN};
		if (poll(&p, 1, 20) != 1) {
			continue;
		}
		int fd = accept(m->listener, nullptr, nullptr);
		if (fd < 0) {
			continue;
		}
		struct handler *h = xmalloc(sizeof(*h));
		*h = (struct handler){.m = m, .fd = fd};
		pthread_t t;
		assert_int_equal(pthread_create(&t, nullptr, handle, h), 0);
		pthread_mutex_lock(&m->mu);
		vec_push(&m->handlers, t);
		pthread_mutex_unlock(&m->mu);
	}
	return nullptr;
}

static void free_string(void *p) {
	xfree(p);
}

struct turso_mock *turso_mock_start(void) {
	struct turso_mock *m = xcalloc(1, sizeof(*m));
	pthread_mutex_init(&m->mu, nullptr);
	strmap_init(&m->st.databases, free_string);
	strmap_init(&m->st.streams, stream_free);
	m->st.close_idle_streams = true;
	char tmpl[] = "/tmp/soju-turso-XXXXXX";
	assert_non_null(mkdtemp(tmpl));
	m->root = xstrdup(tmpl);
	m->listener = socket(AF_INET, SOCK_STREAM, 0);
	if (m->listener < 0) {
		abort();
	}
	struct sockaddr_in addr = {.sin_family = AF_INET,
				   .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
	assert_int_equal(bind(m->listener, (struct sockaddr *)&addr, sizeof(addr)), 0);
	assert_int_equal(listen(m->listener, 64), 0);
	socklen_t len = sizeof(addr);
	assert_int_equal(getsockname(m->listener, (struct sockaddr *)&addr, &len), 0);
	snprintf(m->url, sizeof(m->url), "http://127.0.0.1:%u", (unsigned)ntohs(addr.sin_port));
	assert_int_equal(pthread_create(&m->thread, nullptr, serve, m), 0);
	return m;
}

size_t turso_mock_count(struct turso_mock *m, const char *sql) {
	pthread_mutex_lock(&m->mu);
	size_t n = 0;
	for (size_t i = 0; i < m->st.requests.len; i++) {
		if (strstr(m->st.requests.data[i], sql) != nullptr) {
			n++;
		}
	}
	pthread_mutex_unlock(&m->mu);
	return n;
}

sqlite3 *turso_mock_open(struct turso_mock *m, const char *name) {
	return open_db(m, name);
}

void turso_mock_stop(struct turso_mock *m) {
	pthread_mutex_lock(&m->mu);
	m->stop = true;
	pthread_mutex_unlock(&m->mu);
	pthread_join(m->thread, nullptr);
	for (size_t i = 0; i < m->handlers.len; i++) {
		pthread_join(m->handlers.data[i], nullptr);
	}
	vec_free(&m->handlers);
	close(m->listener);
	strmap_free(&m->st.streams);
	const char *name;
	for (size_t it = 0; strmap_next(&m->st.databases, &it, &name, nullptr);) {
		AUTOFREE char *path = xasprintf("%s/%s", m->root, name);
		unlink(path);
	}
	strmap_free(&m->st.databases);
	rmdir(m->root);
	xfree(m->root);
	xfree(m->st.fail_sql);
	xfree(m->st.fault_sql);
	xfree(m->st.delay_sql);
	xfree(m->st.delay_before_sql);
	vec_free(&m->st.request_sizes);
	strvec_free(&m->st.requests);
	pthread_mutex_destroy(&m->mu);
	xfree(m);
}
