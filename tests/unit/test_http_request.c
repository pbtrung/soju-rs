// C-only tests of the HTTP/1.1 request parser (soju-rs relied on hyper)
#include "testsupport.h"

#include "base/alloc.h"
#include "http/request.h"

static enum http_parse_result parse(const char *s, struct http_request *req, size_t *used) {
	return http_parse_request(str_from(s), req, used);
}

static void test_request(void **state) {
	(void)state;
	const char *s = "GET /socket?x=1 HTTP/1.1\r\n"
			"Host: example.org\r\n"
			"Connection: keep-alive, Upgrade\r\n"
			"X-Forwarded-For: 1.2.3.4\r\n"
			"x-forwarded-for:  5.6.7.8 \r\n"
			"\r\n"
			"rest";
	struct http_request req;
	size_t used;
	assert_int_equal(parse(s, &req, &used), HTTP_REQUEST);
	assert_int_equal(used, strlen(s) - 4);
	assert_string_equal(req.method, "GET");
	assert_string_equal(req.target, "/socket?x=1");
	assert_str_eq(req.path, "/socket");
	assert_int_equal(req.minor, 1);
	assert_true(req.keep_alive);
	assert_string_equal(http_header(&req, "HOST"), "example.org");
	assert_true(http_header_has_token(&req, "connection", "upgrade"));
	assert_false(http_header_has_token(&req, "connection", "close"));
	struct header_values xff = http_header_values(&req, "X-Forwarded-For");
	assert_int_equal(xff.n, 2);
	assert_str_eq(xff.v[1], "5.6.7.8");
	http_header_values_free(xff);
	assert_null(http_header(&req, "Origin"));
	http_request_free(&req);
}

static void test_incomplete(void **state) {
	(void)state;
	const char *s = "GET / HTTP/1.1\r\nHost: x\r\n\r\n";
	struct http_request req;
	size_t used;
	for (size_t n = 0; n < strlen(s); n++) {
		assert_int_equal(http_parse_request(str_new(s, n), &req, &used), HTTP_NEED_MORE);
	}
	// LF line endings are accepted too
	assert_int_equal(parse("GET / HTTP/1.0\nHost: x\n\n", &req, &used), HTTP_REQUEST);
	assert_false(req.keep_alive);
	http_request_free(&req);
}

static void test_framing(void **state) {
	(void)state;
	struct http_request req;
	size_t used;
	assert_int_equal(parse("POST / HTTP/1.1\r\nContent-Length: 12\r\nConnection: close\r\n\r\n",
			       &req, &used),
			 HTTP_REQUEST);
	assert_int_equal(req.content_length, 12);
	assert_false(req.keep_alive);
	http_request_free(&req);
	assert_int_equal(
		parse("POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n", &req, &used),
		HTTP_REQUEST);
	assert_true(req.chunked);
	http_request_free(&req);
	static const char *const bad[] = {
		"POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
		"POST / HTTP/1.1\r\nContent-Length: +1\r\n\r\n",
		"POST / HTTP/1.1\r\nContent-Length: 99999999999999999999999\r\n\r\n",
		"POST / HTTP/1.1\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
		"GET / HTTP/2.0\r\n\r\n",
		"GET /\r\n\r\n",
		"GET  / HTTP/1.1\r\n\r\n",
		"G@T / HTTP/1.1\r\n\r\n",
		"GET / HTTP/1.1\r\nBad Name: x\r\n\r\n",
		"GET / HTTP/1.1\r\nNoColon\r\n\r\n",
		"GET / HTTP/1.1\r\nA: b\r\n folded\r\n\r\n",
		"GET / HTTP/1.1\r\nA: b\x01\r\n\r\n",
	};
	for (size_t i = 0; i < sizeof(bad) / sizeof(bad[0]); i++) {
		assert_int_equal(parse(bad[i], &req, &used), HTTP_BAD_REQUEST);
	}
}

static void test_limits(void **state) {
	(void)state;
	struct http_request req;
	size_t used;
	AUTO_STRBUF struct strbuf s = STRBUF_INIT;
	strbuf_append_cstr(&s, "GET / HTTP/1.1\r\n");
	for (size_t i = 0; i < HTTP_MAX_HEADERS + 1; i++) {
		strbuf_printf(&s, "X-%zu: y\r\n", i);
	}
	strbuf_append_cstr(&s, "\r\n");
	assert_int_equal(http_parse_request(strbuf_str(&s), &req, &used), HTTP_HEADERS_TOO_LARGE);
	// A header section that never ends
	strbuf_reset(&s);
	strbuf_append_cstr(&s, "GET / HTTP/1.1\r\nX: ");
	while (s.len <= HTTP_MAX_HEADER_BYTES) {
		strbuf_append_char(&s, 'y');
	}
	assert_int_equal(http_parse_request(strbuf_str(&s), &req, &used), HTTP_HEADERS_TOO_LARGE);
}

T_MAIN(cmocka_unit_test(test_request), cmocka_unit_test(test_incomplete),
       cmocka_unit_test(test_framing), cmocka_unit_test(test_limits))
