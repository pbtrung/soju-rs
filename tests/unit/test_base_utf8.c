// Tests of src/base/utf8.c against Rust's str semantics (no Rust
// counterpart: soju-rs uses std)
#include "testsupport.h"

#include "base/alloc.h"
#include "base/utf8.h"

static void lossy_eq(const char *in, size_t len, const char *want) {
	struct strbuf sb = STRBUF_INIT;
	utf8_lossy(&sb, str_new(in, len));
	assert_sb_eq(&sb, want);
	strbuf_free(&sb);
}

static void test_lossy(void **state) {
	(void)state;
	lossy_eq("abc", 3, "abc");
	// One U+FFFD per maximal invalid subpart, like String::from_utf8_lossy
	lossy_eq("a\xE2\x82z", 4, "a\xEF\xBF\xBDz");
	lossy_eq("\xF0\x9F\x98", 3, "\xEF\xBF\xBD");
	lossy_eq("\xC0\xAF", 2, "\xEF\xBF\xBD\xEF\xBF\xBD");
	lossy_eq("\xED\xA0\x80", 3, "\xEF\xBF\xBD\xEF\xBF\xBD\xEF\xBF\xBD");
	lossy_eq("\xEF\xBF", 2, "\xEF\xBF\xBD");
	lossy_eq("\xEF\xBF\xBD", 3, "\xEF\xBF\xBD");
	lossy_eq("", 0, "");
	assert_false(utf8_valid(str_new("\xF4\x90\x80\x80", 4)));
	assert_true(utf8_valid(str_new("\xF4\x8F\xBF\xBF", 4)));
}

static void case_eq(bool upper, const char *in, const char *want) {
	struct strbuf sb = STRBUF_INIT;
	if (upper) {
		utf8_to_upper(&sb, str_from(in));
	} else {
		utf8_to_lower(&sb, str_from(in));
	}
	assert_sb_eq(&sb, want);
	strbuf_free(&sb);
}

static void test_case(void **state) {
	(void)state;
	case_eq(true,
		"stra\xC3\x9F"
		"e",
		"STRASSE");
	case_eq(true, "\xEF\xAC\x83", "FFI");
	case_eq(false, "ABC\xC3\x89", "abc\xC3\xA9");
	// Final sigma
	case_eq(false, "\xCE\x9F\xCE\xA3", "\xCE\xBF\xCF\x82");
	case_eq(false, "\xCE\x9F\xCE\xA3\xCE\x9F", "\xCE\xBF\xCF\x83\xCE\xBF");
	case_eq(false, "\xCE\xA3", "\xCF\x83");
	case_eq(false, "\xCE\x9F\xCE\xA3 x", "\xCE\xBF\xCF\x82 x");
	case_eq(false, "\xC4\xB0", "i\xCC\x87");
}

static void debug_eq(const char *in, const char *want) {
	char *s = utf8_debug_cstr(str_from(in));
	assert_string_equal(s, want);
	xfree(s);
}

static void test_debug(void **state) {
	(void)state;
	debug_eq("a\"b\\c'", "\"a\\\"b\\\\c'\"");
	debug_eq("\t\r\n", "\"\\t\\r\\n\"");
	debug_eq("\x01", "\"\\u{1}\"");
	debug_eq("\xC3\xA9", "\"\xC3\xA9\"");
	debug_eq("\xE2\x80\x8B", "\"\\u{200b}\"");
	debug_eq("\xCC\x81", "\"\\u{301}\"");
	debug_eq(" ", "\" \"");
	debug_eq("\xC2\xA0", "\"\\u{a0}\"");
}

static void test_properties(void **state) {
	(void)state;
	assert_true(utf8_is_alphabetic(0x3B1));
	assert_false(utf8_is_alphabetic('1'));
	assert_true(utf8_is_numeric(0x00BD));
	assert_true(utf8_is_whitespace(0x3000));
	assert_false(utf8_is_whitespace(0x200B));
	size_t i = 3;
	uint32_t cp;
	assert_true(utf8_prev(str_new("a\xC3\xA9", 3), &i, &cp));
	assert_int_equal(cp, 0xE9);
	assert_int_equal(i, 1);
}

T_MAIN(cmocka_unit_test(test_lossy), cmocka_unit_test(test_case), cmocka_unit_test(test_debug),
       cmocka_unit_test(test_properties))
