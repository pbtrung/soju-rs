// Ported from soju-rs src/scfg.rs tests
#include "testsupport.h"

#include "config/scfg.h"

static void test_basic(void **state) {
	(void)state;
	struct scfg_block b;
	assert_true(
		scfg_parse(STR_LIT("# comment\nlisten ircs://\n\ntls \"cert file.pem\" key.pem\n"
				   "block a {\n  child x\n}\nempty {\n}\n"),
			   &b, nullptr));
	assert_int_equal(b.v.len, 4);
	assert_string_equal(b.v.data[0].name, "listen");
	assert_int_equal(b.v.data[0].params.len, 1);
	assert_string_equal(b.v.data[0].params.data[0], "ircs://");
	assert_int_equal(b.v.data[1].params.len, 2);
	assert_string_equal(b.v.data[1].params.data[0], "cert file.pem");
	assert_string_equal(b.v.data[1].params.data[1], "key.pem");
	assert_int_equal(b.v.data[2].params.len, 1);
	assert_string_equal(b.v.data[2].params.data[0], "a");
	assert_true(b.v.data[2].has_children);
	assert_string_equal(b.v.data[2].children.v.data[0].name, "child");
	assert_true(b.v.data[3].has_children);
	assert_int_equal(b.v.data[3].children.v.len, 0);
	assert_false(b.v.data[0].has_children);
	scfg_block_free(&b);
}

static void words_eq(const char *l, const char *const *want, size_t n) {
	strvec w = {0};
	assert_true(scfg_split_words(str_from(l), &w, nullptr));
	assert_int_equal(w.len, n);
	for (size_t i = 0; i < n; i++) {
		assert_string_equal(w.data[i], want[i]);
	}
	strvec_free(&w);
}

static void words_err(const char *l) {
	strvec w = {0};
	struct err *e = nullptr;
	assert_false(scfg_split_words(str_from(l), &w, &e));
	assert_int_equal(w.len, 0);
	err_free(e);
}

static void test_quoting(void **state) {
	(void)state;
	words_eq("a \"b c\" 'd' \"\" e\\ f", (const char *[]){"a", "b c", "d", "", "e f"}, 5);
	words_err("a \"b\"c");
	words_err("a b\"c\"");
	words_err("a \"b");
	words_eq("a #b", (const char *[]){"a", "#b"}, 2);
}

static void parse_err(const char *s, const char *want) {
	struct scfg_block b;
	struct err *e = nullptr;
	assert_false(scfg_parse(str_from(s), &b, &e));
	assert_string_equal(err_msg(e), want);
	err_free(e);
}

static void test_errors(void **state) {
	(void)state;
	parse_err("}\n", "line 1: unexpected '}'");
	parse_err("a {\n", "line 1: unterminated block");
	// C-only: messages of the other errors
	parse_err("a \"b\n", "line 1: unterminated quoted string");
}

// C-only: the nesting limit and CRLF line ends
static void test_depth_crlf(void **state) {
	(void)state;
	struct strbuf s = STRBUF_INIT;
	for (int i = 0; i < 1001; i++) {
		strbuf_append_cstr(&s, "a {\n");
	}
	struct scfg_block b;
	struct err *e = nullptr;
	assert_false(scfg_parse(strbuf_str(&s), &b, &e));
	assert_string_equal(err_msg(e), "exceeded max block depth");
	err_free(e);
	strbuf_free(&s);
	assert_true(scfg_parse(STR_LIT("a b\r\nc {\r\n}\r\n"), &b, nullptr));
	assert_int_equal(b.v.len, 2);
	assert_string_equal(b.v.data[0].params.data[0], "b");
	assert_true(b.v.data[1].has_children);
	scfg_block_free(&b);
}

T_MAIN(cmocka_unit_test(test_basic), cmocka_unit_test(test_quoting), cmocka_unit_test(test_errors),
       cmocka_unit_test(test_depth_crlf))
