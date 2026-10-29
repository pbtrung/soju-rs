// Server times: ported from soju-rs src/xirc/mod.rs tests (server_time),
// plus C-only cases of chrono's parsing rules
#include "testsupport.h"

#include "base/alloc.h"
#include "base/timeutil.h"

static struct timestamp ymd(int64_t y, unsigned mo, unsigned d, unsigned h, unsigned mi, unsigned s,
			    uint32_t nsec) {
	return (struct timestamp){
		.sec = days_from_civil(y, mo, d) * 86400 + (int64_t)h * 3600 + (int64_t)mi * 60 + s,
		.nsec = nsec,
	};
}

static void assert_parses(const char *s, struct timestamp want) {
	struct timestamp t;
	assert_true(parse_server_time(str_from(s), &t));
	assert_int_equal(t.sec, want.sec);
	assert_int_equal(t.nsec, want.nsec);
}

static void assert_rejects(const char *s) {
	struct timestamp t;
	assert_false(parse_server_time(str_from(s), &t));
}

static void test_server_time(void **state) {
	(void)state;
	struct timestamp t = ymd(2024, 1, 2, 3, 4, 5, 678000000);
	char *f = format_server_time_cstr(t);
	assert_string_equal(f, "2024-01-02T03:04:05.678Z");
	xfree(f);
	assert_parses("2024-01-02T03:04:05.678Z", t);
	assert_parses("2024-01-02T03:04:05Z", ymd(2024, 1, 2, 3, 4, 5, 0));
	assert_parses("2024-01-02T03:04:05.6Z", ymd(2024, 1, 2, 3, 4, 5, 600000000));
	assert_rejects("2024-01-02T03:04:05.678");
	assert_rejects("garbage");
}

static void test_server_time_chrono_rules(void **state) {
	(void)state;
	assert_int_equal(ymd(1970, 1, 1, 0, 0, 0, 0).sec, 0);
	// Whitespace before numeric fields, single digits, 19 bytes in all
	assert_parses("2024- 1-02T03:04:05Z", ymd(2024, 1, 2, 3, 4, 5, 0));
	assert_parses("  2024-1-2T03:04:05Z", ymd(2024, 1, 2, 3, 4, 5, 0));
	assert_rejects(" 2024-1-2T3:04:05.5Z");
	// A leap second is kept as nanoseconds above 1e9
	assert_parses("2016-12-31T23:59:60.5Z", ymd(2016, 12, 31, 23, 59, 59, 1500000000));
	char *f = format_server_time_cstr(ymd(2016, 12, 31, 23, 59, 59, 1500000000));
	assert_string_equal(f, "2016-12-31T23:59:60.500Z");
	xfree(f);
	assert_rejects("2024-01-02T03:04:05.Z");
	assert_rejects("2024-01-02T03:04:05.1234567890Z");
	assert_rejects("2024-02-30T03:04:05Z");
	assert_rejects("2023-02-29T03:04:05Z");
	assert_parses("2024-02-29T03:04:05Z", ymd(2024, 2, 29, 3, 4, 5, 0));
	assert_rejects("2024-01-02T24:04:05Z");
	assert_rejects("20240-1-02T03:04:5Z");
	assert_rejects("2024-01-02 03:04:05Z");
	// Negative years don't fit in 19 bytes with a 4-digit year
	assert_rejects("-0001-01-01T00:00:00Z");
	assert_parses("-001-01-01T00:00:00Z", ymd(-1, 1, 1, 0, 0, 0, 0));
	f = format_server_time_cstr(ymd(-1, 1, 1, 0, 0, 0, 0));
	assert_string_equal(f, "-0001-01-01T00:00:00.000Z");
	xfree(f);
	f = format_server_time_cstr(ymd(12345, 1, 1, 0, 0, 0, 0));
	assert_string_equal(f, "+12345-01-01T00:00:00.000Z");
	xfree(f);
}

T_MAIN(cmocka_unit_test(test_server_time), cmocka_unit_test(test_server_time_chrono_rules))
