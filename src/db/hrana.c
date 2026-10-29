#include "db/hrana.h"

#include <curl/curl.h>
#include <pthread.h>
#include <string.h>
#include <yyjson.h>

#include "base/alloc.h"
#include "base/base64.h"
#include "base/utf8.h"
#include "db/conn.h"
#include "http/client.h"
#include "net/url.h"

// soju-rs's Turso HTTP client timeout
static constexpr long HRANA_TIMEOUT_MS = 30000;

struct hrana_shared {
	atomic_uint refs;
	pthread_mutex_t mu;
	bool has_index;
	uint64_t index;
	atomic_uint_fast64_t read, written;
};

struct hrana {
	struct hrana_shared *shared;
	char *url, *stream_url, *token;
	char *baton; // the open stream, or nullptr
	bool transaction, transaction_failed, foreign_keys;
	CURL *curl; // kept for connection reuse
};

struct hrana_shared *hrana_shared_new(void) {
	struct hrana_shared *s = xcalloc(1, sizeof(*s));
	atomic_init(&s->refs, 1);
	pthread_mutex_init(&s->mu, nullptr);
	return s;
}

void hrana_shared_unref(struct hrana_shared *s) {
	if (s == nullptr || atomic_fetch_sub(&s->refs, 1) != 1) {
		return;
	}
	pthread_mutex_destroy(&s->mu);
	xfree(s);
}

void hrana_shared_usage(struct hrana_shared *s, uint64_t *read, uint64_t *written) {
	*read = atomic_load(&s->read);
	*written = atomic_load(&s->written);
}

void hrana_stmt_free(struct hrana_stmt *s) {
	xfree(s->sql);
	for (size_t i = 0; i < s->binds.len; i++) {
		xfree(s->binds.data[i].name);
		db_value_free(&s->binds.data[i].v);
	}
	vec_free(&s->binds);
	*s = (struct hrana_stmt){0};
}

struct hrana *hrana_new(const char *url, const char *token, struct hrana_shared *shared) {
	struct hrana *h = xcalloc(1, sizeof(*h));
	h->shared = shared;
	atomic_fetch_add(&shared->refs, 1);
	h->url = xstrdup(url);
	h->stream_url = xstrdup(url);
	h->token = xstrdup(token);
	h->foreign_keys = true;
	return h;
}

struct hrana *hrana_fork(const struct hrana *h) {
	return hrana_new(h->url, h->token, h->shared);
}

static void discard_stream(struct hrana *h);

void hrana_free(struct hrana *h) {
	if (h == nullptr) {
		return;
	}
	discard_stream(h);
	hrana_shared_unref(h->shared);
	xfree(h->url);
	xfree(h->stream_url);
	explicit_bzero(h->token, strlen(h->token));
	xfree(h->token);
	if (h->curl != nullptr) {
		curl_easy_cleanup(h->curl);
	}
	xfree(h);
}

// HTTP

static size_t collect(char *p, size_t size, size_t n, void *data) {
	strbuf_append(data, p, size * n);
	return size * n;
}

// Adds the rows read and written of every result once (soju-rs Usage)
static void observe(struct hrana_shared *s, yyjson_val *root) {
	if (s == nullptr) {
		return;
	}
	uint64_t read = 0, written = 0;
	yyjson_val *results = yyjson_obj_get(root, "results");
	size_t i, n;
	yyjson_val *r;
	yyjson_arr_foreach(results, i, n, r) {
		yyjson_val *result = yyjson_obj_get(yyjson_obj_get(r, "response"), "result");
		yyjson_val *steps = yyjson_obj_get(result, "step_results");
		size_t j, m;
		yyjson_val *step;
		if (yyjson_is_arr(steps)) {
			yyjson_arr_foreach(steps, j, m, step) {
				read += yyjson_get_uint(yyjson_obj_get(step, "rows_read"));
				written += yyjson_get_uint(yyjson_obj_get(step, "rows_written"));
			}
		} else {
			read += yyjson_get_uint(yyjson_obj_get(result, "rows_read"));
			written += yyjson_get_uint(yyjson_obj_get(result, "rows_written"));
		}
	}
	atomic_fetch_add(&s->read, read);
	atomic_fetch_add(&s->written, written);
}

static bool http(CURL *curl, struct hrana_shared *usage, const char *method, const char *url,
		 const char *token, const char *body, size_t body_len, long *status,
		 yyjson_doc **out, struct err **err) {
	http_client_global_init();
	*out = nullptr;
	AUTO_STRBUF struct strbuf resp = STRBUF_INIT;
	char errbuf[CURL_ERROR_SIZE] = "";
	AUTOFREE char *auth = xasprintf("Authorization: Bearer %s", token);
	struct curl_slist *headers = curl_slist_append(nullptr, auth);
	explicit_bzero(auth, strlen(auth));
	if (body != nullptr) {
		headers = curl_slist_append(headers, "Content-Type: application/json");
	}
	headers = curl_slist_append(headers, "Expect:");
	curl_easy_reset(curl);
	curl_easy_setopt(curl, CURLOPT_URL, url);
	curl_easy_setopt(curl, CURLOPT_NOSIGNAL, 1L);
	curl_easy_setopt(curl, CURLOPT_PROTOCOLS_STR, "http,https");
	curl_easy_setopt(curl, CURLOPT_TIMEOUT_MS, HRANA_TIMEOUT_MS);
	curl_easy_setopt(curl, CURLOPT_ERRORBUFFER, errbuf);
	curl_easy_setopt(curl, CURLOPT_HTTPHEADER, headers);
	curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, collect);
	curl_easy_setopt(curl, CURLOPT_WRITEDATA, &resp);
	curl_easy_setopt(curl, CURLOPT_CUSTOMREQUEST, method);
	if (body != nullptr) {
		curl_easy_setopt(curl, CURLOPT_POSTFIELDSIZE_LARGE, (curl_off_t)body_len);
		curl_easy_setopt(curl, CURLOPT_POSTFIELDS, body);
	} else if (cstr_eq(method, "GET")) {
		curl_easy_setopt(curl, CURLOPT_HTTPGET, 1L);
	}
	CURLcode rc = curl_easy_perform(curl);
	curl_slist_free_all(headers);
	if (rc == CURLE_OPERATION_TIMEDOUT) {
		return err_set(err, 0, "Turso HTTP request timed out");
	}
	if (rc != CURLE_OK) {
		return err_set(err, 0, "error sending request for url (%s): %s", url,
			       errbuf[0] != '\0' ? errbuf : curl_easy_strerror(rc));
	}
	curl_easy_getinfo(curl, CURLINFO_RESPONSE_CODE, status);
	// Response bodies of errors aren't kept: they may contain SQL
	// arguments or credentials returned by a proxy
	if (*status < 200 || *status > 299 || resp.len == 0) {
		return true;
	}
	yyjson_read_err rerr;
	*out = yyjson_read_opts(resp.buf, resp.len, 0, nullptr, &rerr);
	if (*out == nullptr) {
		return err_set(err, 0, "%s at line 1 column %zu", rerr.msg, rerr.pos + 1);
	}
	observe(usage, yyjson_doc_get_root(*out));
	return true;
}

void hrana_observe(struct hrana_shared *s, yyjson_doc *doc) {
	observe(s, yyjson_doc_get_root(doc));
}

bool hrana_http(struct hrana_shared *usage, const char *method, const char *url, const char *token,
		const char *body, size_t body_len, long *status, yyjson_doc **out,
		struct err **err) {
	http_client_global_init();
	CURL *curl = curl_easy_init();
	if (curl == nullptr) {
		die_oom();
	}
	bool ok = http(curl, usage, method, url, token, body, body_len, status, out, err);
	curl_easy_cleanup(curl);
	return ok;
}

bool hrana_stream_endpoint(const char *base, char **out, struct err **err) {
	struct url u;
	if (!url_parse(str_from(base), &u, nullptr)) {
		return err_set(err, 0, "invalid Turso stream base URL");
	}
	char *host = nullptr, *port = nullptr;
	if (!split_host_port(str_from(u.host), &host, &port, nullptr)) {
		host = xstrdup(u.host);
	}
	xfree(port);
	bool ok = cstr_eq(u.scheme, "https") && !u.has_user &&
		  str_ends_with(str_from(host), STR_LIT(".turso.io"));
	xfree(host);
	if (!ok) {
		url_free(&u);
		return err_set(err, 0, "invalid Turso stream base URL");
	}
	// Resolved like a relative reference "v2/pipeline"
	const char *slash = strrchr(u.path, '/');
	int dir = slash != nullptr ? (int)(slash - u.path) + 1 : 0;
	*out = xasprintf("https://%s%s%.*sv2/pipeline", u.host, dir == 0 ? "/" : "", dir, u.path);
	url_free(&u);
	return true;
}

// Values

static void encode_value(yyjson_mut_doc *d, yyjson_mut_val *obj, const struct db_value *v) {
	switch (v->type) {
	case DB_NULL:
		yyjson_mut_obj_add_str(d, obj, "type", "null");
		break;
	case DB_INTEGER: {
		char n[32];
		snprintf(n, sizeof(n), "%lld", (long long)v->i);
		yyjson_mut_obj_add_str(d, obj, "type", "integer");
		yyjson_mut_obj_add_strcpy(d, obj, "value", n);
		break;
	}
	case DB_FLOAT:
		yyjson_mut_obj_add_str(d, obj, "type", "float");
		yyjson_mut_obj_add_real(d, obj, "value", v->f);
		break;
	case DB_TEXT: {
		struct strbuf text = STRBUF_INIT;
		utf8_lossy(&text, strbuf_str(&v->b));
		yyjson_mut_obj_add_str(d, obj, "type", "text");
		yyjson_mut_obj_add_strncpy(d, obj, "value", text.buf != nullptr ? text.buf : "",
					   text.len);
		strbuf_free(&text);
		break;
	}
	case DB_BLOB: {
		AUTO_STRBUF struct strbuf b64 = STRBUF_INIT;
		base64_encode(&b64, BASE64_STANDARD, v->b.buf, v->b.len);
		while (b64.len > 0 && b64.buf[b64.len - 1] == '=') {
			strbuf_truncate(&b64, b64.len - 1);
		}
		yyjson_mut_obj_add_str(d, obj, "type", "blob");
		yyjson_mut_obj_add_strncpy(d, obj, "base64", b64.buf != nullptr ? b64.buf : "",
					   b64.len);
		break;
	}
	}
}

static bool decode_value(yyjson_val *v, struct db_value *out) {
	*out = (struct db_value){.type = DB_NULL};
	const char *type = yyjson_get_str(yyjson_obj_get(v, "type"));
	yyjson_val *value = yyjson_obj_get(v, "value");
	if (type == nullptr) {
		return false;
	}
	if (cstr_eq(type, "null")) {
		return true;
	}
	if (cstr_eq(type, "integer")) {
		long long n;
		if (!yyjson_is_str(value) || !str_parse_i64(str_from(yyjson_get_str(value)), &n)) {
			return false;
		}
		*out = (struct db_value){.type = DB_INTEGER, .i = n};
		return true;
	}
	if (cstr_eq(type, "float")) {
		if (!yyjson_is_num(value)) {
			return false;
		}
		*out = (struct db_value){.type = DB_FLOAT, .f = yyjson_get_num(value)};
		return true;
	}
	if (cstr_eq(type, "text")) {
		if (!yyjson_is_str(value)) {
			return false;
		}
		out->type = DB_TEXT;
		strbuf_append(&out->b, yyjson_get_str(value), yyjson_get_len(value));
		return true;
	}
	if (cstr_eq(type, "blob")) {
		yyjson_val *b64 = yyjson_obj_get(v, "base64");
		if (!yyjson_is_str(b64)) {
			return false;
		}
		// Padding is optional
		AUTO_STRBUF struct strbuf s = STRBUF_INIT;
		strbuf_append(&s, yyjson_get_str(b64), yyjson_get_len(b64));
		while (s.len > 0 && s.buf[s.len - 1] == '=') {
			strbuf_truncate(&s, s.len - 1);
		}
		while (s.len % 4 != 0) {
			strbuf_append_char(&s, '=');
		}
		out->type = DB_BLOB;
		if (!base64_decode(&out->b, BASE64_STANDARD, strbuf_str(&s), nullptr)) {
			db_value_free(out);
			return false;
		}
		return true;
	}
	return false;
}

// {"sql":..., "args":[...], "named_args":[...], "want_rows":...}
static yyjson_mut_val *statement(yyjson_mut_doc *d, const char *sql, const struct db_bind *binds,
				 size_t n, bool want_rows) {
	yyjson_mut_val *stmt = yyjson_mut_obj(d);
	yyjson_mut_obj_add_strcpy(d, stmt, "sql", sql);
	yyjson_mut_obj_add_bool(d, stmt, "want_rows", want_rows);
	int max_index = 0;
	bool named = false;
	for (size_t i = 0; i < n; i++) {
		if (binds[i].name != nullptr) {
			named = true;
		} else if (binds[i].index > max_index) {
			max_index = binds[i].index;
		}
	}
	if (max_index > 0) {
		yyjson_mut_val *args = yyjson_mut_obj_add_arr(d, stmt, "args");
		for (int index = 1; index <= max_index; index++) {
			yyjson_mut_val *v = yyjson_mut_arr_add_obj(d, args);
			const struct db_value null = {.type = DB_NULL}, *value = &null;
			for (size_t i = 0; i < n; i++) {
				if (binds[i].name == nullptr && binds[i].index == index) {
					value = &binds[i].v;
				}
			}
			encode_value(d, v, value);
		}
	}
	if (named) {
		yyjson_mut_val *args = yyjson_mut_obj_add_arr(d, stmt, "named_args");
		for (size_t i = 0; i < n; i++) {
			if (binds[i].name == nullptr) {
				continue;
			}
			yyjson_mut_val *a = yyjson_mut_arr_add_obj(d, args);
			yyjson_mut_obj_add_strcpy(d, a, "name", binds[i].name);
			encode_value(d, yyjson_mut_obj_add_obj(d, a, "value"), &binds[i].v);
		}
	}
	return stmt;
}

// Pipelines

// soju-rs pipeline_result: the response of one request, or an error
static yyjson_val *pipeline_result(yyjson_val *result, const char *expected, struct err **err) {
	const char *type = yyjson_get_str(yyjson_obj_get(result, "type"));
	if (type != nullptr && cstr_eq(type, "error")) {
		yyjson_val *e = yyjson_obj_get(result, "error");
		const char *code = yyjson_get_str(yyjson_obj_get(e, "code"));
		const char *msg = yyjson_get_str(yyjson_obj_get(e, "message"));
		code = code != nullptr ? code : "UNKNOWN";
		int sqlite_code = cstr_eq(code, "SQLITE_CONSTRAINT_UNIQUE")
					  ? SQLITE_CONSTRAINT_UNIQUE
				  : str_starts_with(str_from(code), STR_LIT("SQLITE_CONSTRAINT"))
					  ? SQLITE_CONSTRAINT
					  : 0;
		err_set(err, sqlite_code, "Turso SQL error (%s): %s", code,
			msg != nullptr ? msg : "unknown error");
		return nullptr;
	}
	yyjson_val *resp = yyjson_obj_get(result, "response");
	const char *rtype = yyjson_get_str(yyjson_obj_get(resp, "type"));
	if (type == nullptr || !cstr_eq(type, "ok") || rtype == nullptr ||
	    !cstr_eq(rtype, expected)) {
		err_set(err, 0, "invalid Turso SQL response type");
		return nullptr;
	}
	return resp;
}

static void set_baton(struct hrana *h, const char *baton) {
	xfree(h->baton);
	h->baton = baton != nullptr ? xstrdup(baton) : nullptr;
}

// Sends a pipeline around one request, on the current stream (or a new
// one); keep: leave the stream open. *doc owns *resp.
static bool pipeline(struct hrana *h, yyjson_mut_doc *d, yyjson_mut_val *request,
		     const char *expected, bool keep, yyjson_doc **doc, yyjson_val **resp,
		     struct err **err) {
	*doc = nullptr;
	yyjson_mut_val *root = yyjson_mut_obj(d);
	yyjson_mut_doc_set_root(d, root);
	if (h->baton != nullptr) {
		yyjson_mut_obj_add_strcpy(d, root, "baton", h->baton);
	} else {
		yyjson_mut_obj_add_null(d, root, "baton");
	}
	yyjson_mut_val *requests = yyjson_mut_obj_add_arr(d, root, "requests");
	size_t count = 0;
	if (h->baton == nullptr) {
		yyjson_mut_val *pragma = yyjson_mut_arr_add_obj(d, requests);
		yyjson_mut_obj_add_str(d, pragma, "type", "sequence");
		yyjson_mut_obj_add_str(d, pragma, "sql",
				       h->foreign_keys ? "PRAGMA foreign_keys = 1"
						       : "PRAGMA foreign_keys = 0");
		count++;
	}
	size_t result_index = count;
	yyjson_mut_arr_append(requests, request);
	count++;
	// Sequence responses have no replication index. Read it on the same
	// stream before closing, so the next stream observes schema writes.
	if (cstr_eq(expected, "sequence")) {
		yyjson_mut_val *probe = yyjson_mut_arr_add_obj(d, requests);
		yyjson_mut_obj_add_str(d, probe, "type", "execute");
		yyjson_mut_val *stmt = yyjson_mut_obj_add_obj(d, probe, "stmt");
		yyjson_mut_obj_add_str(d, stmt, "sql", "SELECT 1");
		count++;
	}
	if (!keep && !cstr_eq(expected, "close")) {
		yyjson_mut_val *close = yyjson_mut_arr_add_obj(d, requests);
		yyjson_mut_obj_add_str(d, close, "type", "close");
		count++;
	}
	size_t len = 0;
	char *body = yyjson_mut_write(d, 0, &len);
	if (body == nullptr) {
		die_oom();
	}
	if (cstr_eq(expected, "batch") && len > HRANA_BODY_BUDGET) {
		free(body);
		return err_set(err, 0, "Turso message batch exceeds HTTP body budget");
	}
	if (h->curl == nullptr && (h->curl = curl_easy_init()) == nullptr) {
		die_oom();
	}
	long status = 0;
	bool ok = http(h->curl, h->shared, "POST", h->stream_url, h->token, body, len, &status, doc,
		       err);
	free(body);
	if (!ok) {
		return false;
	}
	if (status < 200 || status > 299) {
		AUTO_STRBUF struct strbuf st = STRBUF_INIT;
		http_status_text(&st, (int)status);
		return err_set(err, 0, "Turso request failed (HTTP %s)", strbuf_cstr(&st));
	}
	yyjson_val *r = yyjson_doc_get_root(*doc);
	yyjson_val *baton = yyjson_obj_get(r, "baton");
	if (yyjson_is_null(baton)) {
		set_baton(h, nullptr);
	} else if (yyjson_is_str(baton) && yyjson_get_len(baton) > 0) {
		set_baton(h, yyjson_get_str(baton));
	} else {
		return err_set(err, 0, "invalid or missing Turso stream baton");
	}
	yyjson_val *base = yyjson_obj_get(r, "base_url");
	if (yyjson_is_str(base)) {
		char *endpoint = nullptr;
		if (!hrana_stream_endpoint(yyjson_get_str(base), &endpoint, err)) {
			return false;
		}
		xfree(h->stream_url);
		h->stream_url = endpoint;
	} else if (base != nullptr && !yyjson_is_null(base)) {
		return err_set(err, 0, "invalid Turso stream base URL");
	}
	yyjson_val *results = yyjson_obj_get(r, "results");
	if (!yyjson_is_arr(results)) {
		return err_set(err, 0, "missing Turso pipeline results");
	}
	if (yyjson_arr_size(results) != count) {
		return err_set(err, 0, "incomplete Turso pipeline response");
	}
	for (size_t i = 0; i < count; i++) {
		yyjson_mut_val *req = yyjson_mut_arr_get(requests, i);
		const char *type = yyjson_mut_get_str(yyjson_mut_obj_get(req, "type"));
		yyjson_val *payload = pipeline_result(yyjson_arr_get(results, i), type, err);
		if (payload == nullptr) {
			return false;
		}
		yyjson_val *index =
			yyjson_obj_get(yyjson_obj_get(payload, "result"), "replication_index");
		if (yyjson_is_str(index)) {
			unsigned long long n;
			if (!str_parse_u64(str_from(yyjson_get_str(index)), &n)) {
				return err_set(err, 0, "invalid Turso replication index");
			}
			pthread_mutex_lock(&h->shared->mu);
			if (!h->shared->has_index || n > h->shared->index) {
				h->shared->index = n;
			}
			h->shared->has_index = true;
			pthread_mutex_unlock(&h->shared->mu);
		}
	}
	if (keep && h->baton == nullptr) {
		return err_set(err, 0, "Turso closed the stream during a transaction");
	}
	if (!keep && h->baton != nullptr) {
		return err_set(err, 0, "Turso did not close the SQL stream");
	}
	if (h->baton == nullptr) {
		xfree(h->stream_url);
		h->stream_url = xstrdup(h->url);
	}
	*resp = pipeline_result(yyjson_arr_get(results, result_index), expected, err);
	return *resp != nullptr;
}

// Closing a stream rolls back any open transaction. Use the last known
// baton once, then forget it even if the server no longer recognizes it.
static void discard_stream(struct hrana *h) {
	if (h->baton != nullptr) {
		yyjson_mut_doc *d = yyjson_mut_doc_new(nullptr);
		yyjson_mut_val *root = yyjson_mut_obj(d);
		yyjson_mut_doc_set_root(d, root);
		yyjson_mut_obj_add_strcpy(d, root, "baton", h->baton);
		yyjson_mut_val *requests = yyjson_mut_obj_add_arr(d, root, "requests");
		yyjson_mut_obj_add_str(d, yyjson_mut_arr_add_obj(d, requests), "type", "close");
		size_t len = 0;
		char *json = yyjson_mut_write(d, 0, &len);
		yyjson_mut_doc_free(d);
		if (h->curl == nullptr) {
			h->curl = curl_easy_init();
		}
		long status;
		yyjson_doc *doc = nullptr;
		if (json != nullptr && h->curl != nullptr &&
		    http(h->curl, nullptr, "POST", h->stream_url, h->token, json, len, &status,
			 &doc, nullptr)) {
			yyjson_doc_free(doc);
		}
		free(json);
		set_baton(h, nullptr);
	}
	xfree(h->stream_url);
	h->stream_url = xstrdup(h->url);
}

// soju-rs request_with_stream: adds the replication index, and on failure
// poisons the transaction or discards the stream
static bool request(struct hrana *h, yyjson_mut_doc *d, yyjson_mut_val *req, const char *type,
		    bool keep, yyjson_doc **doc, yyjson_val **resp, struct err **err) {
	if (h->transaction_failed) {
		return err_set(err, 0,
			       "Turso transaction failed; discard it before issuing more SQL");
	}
	pthread_mutex_lock(&h->shared->mu);
	bool has_index = h->shared->has_index;
	uint64_t index = h->shared->index;
	pthread_mutex_unlock(&h->shared->mu);
	if (has_index) {
		yyjson_mut_val *target = cstr_eq(type, "execute") ? yyjson_mut_obj_get(req, "stmt")
					 : cstr_eq(type, "batch") ? yyjson_mut_obj_get(req, "batch")
								  : req;
		char n[32];
		snprintf(n, sizeof(n), "%llu", (unsigned long long)index);
		yyjson_mut_obj_add_strcpy(d, target, "replication_index", n);
	}
	if (pipeline(h, d, req, type, keep, doc, resp, err)) {
		return true;
	}
	yyjson_doc_free(*doc);
	*doc = nullptr;
	if (h->transaction) {
		// A failed or malformed response invalidates this transaction.
		// Never continue its SQL on a new auto-commit stream.
		h->transaction_failed = true;
	} else {
		discard_stream(h);
	}
	return false;
}

static bool execute(struct hrana *h, const char *sql, const struct db_bind *binds, size_t n,
		    bool keep, strvec *columns, db_rows *rows, int64_t *last_id, int64_t *changes,
		    struct err **err) {
	yyjson_mut_doc *d = yyjson_mut_doc_new(nullptr);
	yyjson_mut_val *req = yyjson_mut_obj(d);
	yyjson_mut_obj_add_str(d, req, "type", "execute");
	yyjson_mut_obj_add_val(d, req, "stmt", statement(d, sql, binds, n, true));
	yyjson_doc *doc = nullptr;
	yyjson_val *resp = nullptr;
	bool ok = request(h, d, req, "execute", keep, &doc, &resp, err);
	yyjson_mut_doc_free(d);
	if (!ok) {
		return false;
	}
	yyjson_val *result = yyjson_obj_get(resp, "result");
	if (!yyjson_is_obj(result)) {
		yyjson_doc_free(doc);
		return err_set(err, 0, "missing Turso SQL result");
	}
	yyjson_val *id = yyjson_obj_get(result, "last_insert_rowid");
	long long last = 0;
	if (yyjson_is_str(id) && str_parse_i64(str_from(yyjson_get_str(id)), &last) &&
	    last_id != nullptr) {
		*last_id = last;
	}
	yyjson_val *affected = yyjson_obj_get(result, "affected_row_count");
	if (changes != nullptr) {
		if (!yyjson_is_uint(affected) && !yyjson_is_int(affected)) {
			yyjson_doc_free(doc);
			return err_set(err, 0, "invalid Turso affected row count");
		}
		*changes = (int64_t)yyjson_get_uint(affected);
	}
	if (columns != nullptr) {
		size_t i, m;
		yyjson_val *col;
		yyjson_arr_foreach(yyjson_obj_get(result, "cols"), i, m, col) {
			const char *name = yyjson_get_str(yyjson_obj_get(col, "name"));
			strvec_push(columns, name != nullptr ? name : "");
		}
	}
	if (rows != nullptr) {
		yyjson_val *rs = yyjson_obj_get(result, "rows");
		if (!yyjson_is_arr(rs)) {
			yyjson_doc_free(doc);
			return err_set(err, 0, "invalid Turso rows");
		}
		size_t i, m;
		yyjson_val *row;
		yyjson_arr_foreach(rs, i, m, row) {
			if (!yyjson_is_arr(row)) {
				yyjson_doc_free(doc);
				return err_set(err, 0, "invalid Turso row");
			}
			db_row values = {0};
			size_t j, k;
			yyjson_val *v;
			yyjson_arr_foreach(row, j, k, v) {
				struct db_value value;
				if (!decode_value(v, &value)) {
					for (size_t x = 0; x < values.len; x++) {
						db_value_free(&values.data[x]);
					}
					vec_free(&values);
					yyjson_doc_free(doc);
					return err_set(err, 0,
						       "invalid value in Turso SQL response");
				}
				vec_push(&values, value);
			}
			vec_push(rows, values);
		}
	}
	yyjson_doc_free(doc);
	return true;
}

bool hrana_execute(struct hrana *h, const char *sql, const struct db_bind *binds, size_t n,
		   strvec *columns, void *rows, int64_t *last_id, int64_t *changes,
		   struct err **err) {
	return execute(h, sql, binds, n, h->transaction, columns, (db_rows *)rows, last_id, changes,
		       err);
}

bool hrana_sequence(struct hrana *h, const char *sql, struct err **err) {
	// soju-rs pragma_update: the pragma is sent with every new stream
	if (cstr_eq(sql, "PRAGMA foreign_keys = 0") || cstr_eq(sql, "PRAGMA foreign_keys = 1")) {
		h->foreign_keys = sql[strlen(sql) - 1] == '1';
		return true;
	}
	yyjson_mut_doc *d = yyjson_mut_doc_new(nullptr);
	yyjson_mut_val *req = yyjson_mut_obj(d);
	yyjson_mut_obj_add_str(d, req, "type", "sequence");
	yyjson_mut_obj_add_strcpy(d, req, "sql", sql);
	yyjson_doc *doc = nullptr;
	yyjson_val *resp = nullptr;
	bool ok = request(h, d, req, "sequence", h->transaction, &doc, &resp, err);
	yyjson_mut_doc_free(d);
	yyjson_doc_free(doc);
	return ok;
}

bool hrana_begin(struct hrana *h, struct err **err) {
	if (h->transaction) {
		return err_set(err, 0, "nested Turso transaction");
	}
	h->transaction = true;
	if (!execute(h, "BEGIN IMMEDIATE", nullptr, 0, true, nullptr, nullptr, nullptr, nullptr,
		     err)) {
		discard_stream(h);
		h->transaction = h->transaction_failed = false;
		return false;
	}
	return true;
}

static void close_transaction(struct hrana *h) {
	discard_stream(h);
	h->transaction = h->transaction_failed = false;
}

bool hrana_commit(struct hrana *h, struct err **err) {
	// End the stream in the COMMIT pipeline. Hosted Turso can already
	// release it once COMMIT succeeds, so a null baton here is expected.
	// Keep transaction poisoning active until every response has been
	// validated; never replay COMMIT.
	yyjson_mut_doc *d = yyjson_mut_doc_new(nullptr);
	yyjson_mut_val *req = yyjson_mut_obj(d);
	yyjson_mut_obj_add_str(d, req, "type", "execute");
	yyjson_mut_val *stmt = yyjson_mut_obj_add_obj(d, req, "stmt");
	yyjson_mut_obj_add_str(d, stmt, "sql", "COMMIT");
	yyjson_mut_obj_add_bool(d, stmt, "want_rows", false);
	yyjson_doc *doc = nullptr;
	yyjson_val *resp = nullptr;
	bool ok = request(h, d, req, "execute", false, &doc, &resp, err);
	yyjson_mut_doc_free(d);
	yyjson_doc_free(doc);
	close_transaction(h);
	return ok;
}

void hrana_rollback(struct hrana *h) {
	if (h->transaction) {
		close_transaction(h);
	}
}

bool hrana_atomic_batch(struct hrana *h, const struct hrana_stmt *stmts, size_t n,
			struct err **err) {
	yyjson_mut_doc *d = yyjson_mut_doc_new(nullptr);
	yyjson_mut_val *req = yyjson_mut_obj(d);
	yyjson_mut_obj_add_str(d, req, "type", "batch");
	yyjson_mut_val *batch = yyjson_mut_obj_add_obj(d, req, "batch");
	yyjson_mut_val *steps = yyjson_mut_obj_add_arr(d, batch, "steps");
	size_t total = n + 2;
	for (size_t i = 0; i < total; i++) {
		yyjson_mut_val *step = yyjson_mut_arr_add_obj(d, steps);
		if (i > 0) {
			yyjson_mut_val *cond = yyjson_mut_obj_add_obj(d, step, "condition");
			yyjson_mut_obj_add_str(d, cond, "type", "ok");
			yyjson_mut_obj_add_uint(d, cond, "step", i - 1);
		}
		yyjson_mut_val *stmt;
		if (i == 0 || i == total - 1) {
			stmt = yyjson_mut_obj(d);
			yyjson_mut_obj_add_str(d, stmt, "sql",
					       i == 0 ? "BEGIN IMMEDIATE" : "COMMIT");
		} else {
			const struct hrana_stmt *s = &stmts[i - 1];
			stmt = statement(d, s->sql, s->binds.data, s->binds.len, false);
		}
		yyjson_mut_obj_add_val(d, step, "stmt", stmt);
	}
	yyjson_doc *doc = nullptr;
	yyjson_val *resp = nullptr;
	bool ok = request(h, d, req, "batch", h->transaction, &doc, &resp, err);
	yyjson_mut_doc_free(d);
	if (!ok) {
		return false;
	}
	yyjson_val *result = yyjson_obj_get(resp, "result");
	yyjson_val *results = yyjson_obj_get(result, "step_results");
	yyjson_val *errors = yyjson_obj_get(result, "step_errors");
	if (!yyjson_is_arr(results) || yyjson_arr_size(results) != total) {
		yyjson_doc_free(doc);
		return err_set(err, 0, "incomplete Turso batch results");
	}
	if (!yyjson_is_arr(errors) || yyjson_arr_size(errors) != total) {
		yyjson_doc_free(doc);
		return err_set(err, 0, "incomplete Turso batch errors");
	}
	for (size_t i = 0; i < total && ok; i++) {
		yyjson_val *e = yyjson_arr_get(errors, i);
		if (!yyjson_is_null(e)) {
			const char *code = yyjson_get_str(yyjson_obj_get(e, "code"));
			const char *msg = yyjson_get_str(yyjson_obj_get(e, "message"));
			bool foreign_key =
				(code != nullptr &&
				 cstr_eq(code, "SQLITE_CONSTRAINT_FOREIGNKEY")) ||
				(msg != nullptr &&
				 strstr(msg, "FOREIGN KEY constraint failed") != nullptr);
			err_set(err, 0, "Turso message batch failed (%s): %s",
				foreign_key       ? "SQLITE_CONSTRAINT_FOREIGNKEY"
				: code != nullptr ? code
						  : "UNKNOWN",
				msg != nullptr ? msg : "unknown error");
			ok = false;
		} else if (!yyjson_is_obj(yyjson_arr_get(results, i))) {
			err_set(err, 0, "Turso message batch skipped a statement");
			ok = false;
		}
	}
	yyjson_doc_free(doc);
	return ok;
}
