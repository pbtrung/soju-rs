// C-only tests of the HTTP client (soju-rs used reqwest, the C port libcurl):
// the requests sent, redirects, limits, timeouts, cancellation and internal
// address refusal
#include "testsupport.h"

#include <stdlib.h>

#include "base/alloc.h"
#include "http/client.h"
#include "http_mock.h"

struct result {
	bool done;
	int status;
	struct strbuf body;
	char *err;
};

static void on_done(void *data, const struct http_response *resp, const struct err *err) {
	struct result *r = data;
	r->done = true;
	if (resp != nullptr) {
		r->status = resp->status;
		strbuf_append_str(&r->body, strbuf_str(&resp->body));
	} else {
		r->err = xstrdup(err_msg(err));
	}
}

static void result_free(struct result *r) {
	strbuf_free(&r->body);
	xfree(r->err);
}

// Sends req on a new loop and runs it until the request completes
static void run(struct http_client_request *req, struct result *r) {
	*r = (struct result){0};
	uv_loop_t loop;
	uv_loop_init(&loop);
	http_client_send(&loop, req, on_done, r);
	uint64_t deadline = uv_hrtime() + 10000000000ULL;
	while (!r->done && uv_hrtime() < deadline) {
		uv_run(&loop, UV_RUN_ONCE);
	}
	assert_true(r->done);
	uv_run(&loop, UV_RUN_DEFAULT);
	assert_int_equal(uv_loop_close(&loop), 0);
	http_client_request_free(req);
}

static char *url(struct http_mock *m, const char *path) {
	return xasprintf("http://127.0.0.1:%u%s", (unsigned)http_mock_port(m), path);
}

static void test_status_text(void **state) {
	(void)state;
	AUTO_STRBUF struct strbuf st = STRBUF_INIT;
	http_status_text(&st, 403);
	http_status_text(&st, 599);
	assert_sb_eq(&st, "403 Forbidden599 <unknown status code>");
}

// Echoes the request in a chunked response; /redirect/N redirects N times
static void echo(const struct strbuf *req, struct strbuf *resp, void *data) {
	struct http_mock **other = data;
	const char *r = strstr(req->buf, " /redirect/");
	if (r != nullptr) {
		unsigned n = (unsigned)strtoul(r + strlen(" /redirect/"), nullptr, 10);
		if (n == 0) {
			strbuf_append_cstr(resp, "HTTP/1.1 200 OK\r\nContent-Length: ");
			strbuf_printf(resp, "%zu\r\n\r\n%s", req->len, req->buf);
		} else {
			strbuf_printf(resp, "HTTP/1.1 302 Found\r\nLocation: /redirect/%u\r\n\r\n",
				      n - 1);
		}
		return;
	}
	if (strstr(req->buf, " /cross ") != nullptr) {
		AUTOFREE char *to = url(*other, "/redirect/0");
		strbuf_printf(resp, "HTTP/1.1 307 Temporary Redirect\r\nLocation: %s\r\n\r\n", to);
		return;
	}
	if (strstr(req->buf, " /hold ") != nullptr) {
		return;
	}
	if (strstr(req->buf, " /big ") != nullptr) {
		strbuf_printf(resp, "HTTP/1.1 200 OK\r\nContent-Length: %zu\r\n\r\n",
			      HTTP_MAX_RESPONSE_BODY + 1);
		for (size_t i = 0; i <= HTTP_MAX_RESPONSE_BODY; i++) {
			strbuf_append_char(resp, 'x');
		}
		return;
	}
	strbuf_printf(resp, "HTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\n\r\n%zx\r\n",
		      req->len);
	strbuf_append_str(resp, strbuf_str(req));
	strbuf_append_cstr(resp, "\r\n0\r\n\r\n");
}

static void test_requests(void **state) {
	(void)state;
	struct http_mock *other = nullptr;
	struct http_mock *m = http_mock_start(echo, &other);
	other = http_mock_start(echo, &other);
	struct http_client_request req;
	struct result r;
	AUTOFREE char *u = url(m, "/path?q=1");
	http_client_request_init(&req, "POST", u);
	http_client_request_header(&req, "Authorization", "Basic %s", "abc");
	strbuf_append_cstr(&req.body, "token=x");
	run(&req, &r);
	assert_int_equal(r.status, 201);
	AUTOFREE char *host = xasprintf("Host: 127.0.0.1:%u\r\n", (unsigned)http_mock_port(m));
	assert_non_null(strstr(r.body.buf, "POST /path?q=1 HTTP/1.1\r\n"));
	assert_non_null(strstr(r.body.buf, host));
	assert_non_null(strstr(r.body.buf, "Content-Length: 7\r\n"));
	assert_non_null(strstr(r.body.buf, "\r\n\r\ntoken=x"));
	result_free(&r);

	// A POST redirected with 302 becomes a GET without body; same origin:
	// the credentials are kept
	AUTOFREE char *u2 = url(m, "/redirect/3");
	http_client_request_init(&req, "POST", u2);
	http_client_request_header(&req, "Authorization", "Basic %s", "abc");
	strbuf_append_cstr(&req.body, "x");
	run(&req, &r);
	assert_int_equal(r.status, 200);
	assert_non_null(strstr(r.body.buf, "GET /redirect/0 HTTP/1.1\r\n"));
	assert_non_null(strstr(r.body.buf, "Authorization: Basic abc\r\n"));
	result_free(&r);

	// Another origin: they're dropped; 307 keeps the method
	AUTOFREE char *u3 = url(m, "/cross");
	http_client_request_init(&req, "POST", u3);
	http_client_request_header(&req, "Authorization", "Basic %s", "abc");
	run(&req, &r);
	assert_int_equal(r.status, 200);
	assert_non_null(strstr(r.body.buf, "POST /redirect/0 HTTP/1.1\r\n"));
	assert_null(strstr(r.body.buf, "Authorization"));
	result_free(&r);

	AUTOFREE char *u4 = url(m, "/redirect/11");
	http_client_request_init(&req, "GET", u4);
	run(&req, &r);
	assert_non_null(strstr(r.err, "stopped after 10 redirects"));
	result_free(&r);

	AUTOFREE char *big = url(m, "/big");
	http_client_request_init(&req, "GET", big);
	run(&req, &r);
	assert_non_null(r.err);
	result_free(&r);

	AUTOFREE char *u5 = url(m, "/hold");
	http_client_request_init(&req, "GET", u5);
	req.timeout_ms = 200;
	run(&req, &r);
	assert_non_null(strstr(r.err, "operation timed out"));
	result_free(&r);
	http_mock_stop(m);
	http_mock_stop(other);
}

static void test_internal_addresses(void **state) {
	(void)state;
	struct http_mock *m = http_mock_start(echo, nullptr);
	struct http_client_request req;
	struct result r;
	AUTOFREE char *u = url(m, "/");
	AUTOFREE char *by_name = xasprintf("http://localhost:%u/", (unsigned)http_mock_port(m));
	const char *urls[] = {u, by_name};
	for (size_t i = 0; i < 2; i++) {
		http_client_request_init(&req, "GET", urls[i]);
		req.external_only = true;
		run(&req, &r);
		assert_non_null(r.err);
		assert_non_null(strstr(r.err, HTTP_INTERNAL_IP_ERROR));
		result_free(&r);
	}
	assert_int_equal(http_mock_requests(m), 0);
	http_mock_stop(m);
	// Nothing listening
	http_client_request_init(&req, "GET", "http://127.0.0.1:1/");
	run(&req, &r);
	assert_non_null(strstr(r.err, "Failed to connect"));
	result_free(&r);
	http_client_request_init(&req, "GET", "ftp://example.org/");
	run(&req, &r);
	assert_non_null(strstr(r.err, "Protocol \"ftp\" is disabled"));
	result_free(&r);
}

static void test_cancel(void **state) {
	(void)state;
	struct http_mock *m = http_mock_start(echo, nullptr);
	uv_loop_t loop;
	uv_loop_init(&loop);
	struct result r = {0};
	struct http_client_request req;
	AUTOFREE char *u = url(m, "/hold");
	http_client_request_init(&req, "GET", u);
	http_client_send(&loop, &req, on_done, &r);
	http_client_request_free(&req);
	assert_int_equal(http_client_pending(), 1);
	while (http_mock_requests(m) == 0) {
		uv_run(&loop, UV_RUN_NOWAIT);
	}
	http_client_cancel_all();
	assert_true(r.done);
	assert_non_null(strstr(r.err, "operation cancelled"));
	assert_int_equal(http_client_pending(), 0);
	uv_run(&loop, UV_RUN_DEFAULT);
	assert_int_equal(uv_loop_close(&loop), 0);
	result_free(&r);
	http_mock_stop(m);
}

T_MAIN(cmocka_unit_test(test_status_text), cmocka_unit_test(test_requests),
       cmocka_unit_test(test_internal_addresses), cmocka_unit_test(test_cancel))
