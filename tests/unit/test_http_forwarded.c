// Ported from soju-rs src/listen.rs tests (forwarded, client_cert_header)
#include "testsupport.h"

#include "base/alloc.h"
#include "http/forwarded.h"

#define HV(...)                                                    \
	((struct header_values){(const struct str[]){__VA_ARGS__}, \
				sizeof((const struct str[]){__VA_ARGS__}) / sizeof(struct str)})
static const struct header_values NONE = {nullptr, 0};

static void fwd_eq(struct header_values f, struct header_values x, const char *want) {
	char *got = parse_forwarded(f, x);
	assert_cstr_eq(got, want);
	xfree(got);
}

static void test_forwarded(void **state) {
	(void)state;
	fwd_eq(HV(STR_LIT("for=\"[2001:db8::1]:1234\";proto=https")), NONE, "[2001:db8::1]:1234");
	fwd_eq(NONE, HV(STR_LIT("1.2.3.4")), "1.2.3.4");
	// A list: the last element was added by the trusted proxy
	fwd_eq(NONE, HV(STR_LIT("6.6.6.6, 1.2.3.4 , 127.0.0.1")), "127.0.0.1");
	// Several header lines form a single list
	fwd_eq(NONE, HV(STR_LIT("6.6.6.6"), STR_LIT("2001:db8::1,")), "2001:db8::1");
	fwd_eq(NONE, HV(STR_LIT("[2001:db8::1]:443")), "[2001:db8::1]:443");
	// Anything else falls back to the connection's address
	const char *bad[] = {"", " , ", "not an ip", "1.2.3.4 5.6.7.8", "unknown"};
	for (size_t i = 0; i < 5; i++) {
		fwd_eq(NONE, HV(str_from(bad[i])), nullptr);
	}
	fwd_eq(HV(STR_LIT("for=\"a b\"")), NONE, nullptr);
	fwd_eq(HV(STR_LIT("for=1.1.1.1, for=2.2.2.2")), NONE, nullptr);
}

static void test_client_cert_header(void **state) {
	(void)state;
	struct strbuf sb = STRBUF_INIT;
	assert_true(parse_client_cert(STR_LIT(":AAEC:"), &sb));
	assert_int_equal(sb.len, 3);
	assert_memory_equal(sb.buf, "\0\1\2", 3);
	assert_false(parse_client_cert(STR_LIT("AAEC"), &sb));
	strbuf_free(&sb);
}

// C-only: the Origin check
static void origin_err(const char *origin, const char *host, const strvec *patterns,
		       const char *want) {
	struct err *e = nullptr;
	assert_false(authenticate_origin(origin, host, patterns, &e));
	assert_string_equal(err_msg(e), want);
	err_free(e);
}

static void test_origin(void **state) {
	(void)state;
	strvec patterns = {0};
	assert_true(authenticate_origin(nullptr, "h", &patterns, nullptr));
	assert_true(authenticate_origin("https://H:1", "h:1", &patterns, nullptr));
	origin_err("https://evil.example", "h", &patterns,
		   "request Origin \"evil.example\" is not authorized for Host \"h\"");
	strvec_push(&patterns, "*.EXAMPLE");
	assert_true(authenticate_origin("https://good.example", "h", &patterns, nullptr));
	strvec_push(&patterns, "[");
	origin_err("https://x.y", "h", &patterns,
		   "failed to parse path pattern \"[\": syntax error in pattern");
	origin_err("nope", "h", &patterns,
		   "failed to parse Origin header \"nope\": missing scheme");
	strvec_free(&patterns);
}

T_MAIN(cmocka_unit_test(test_forwarded), cmocka_unit_test(test_client_cert_header),
       cmocka_unit_test(test_origin))
