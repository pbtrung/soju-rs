// Ported from soju-rs src/msgstore/mod.rs (go_compat) and
// src/msgstore/bare.rs (roundtrip) tests
#include "testsupport.h"

#include <stdint.h>

#include "base/alloc.h"
#include "msgstore/bare.h"
#include "msgstore/msgid.h"

static void id_eq(char *got, const char *want) {
	assert_string_equal(got, want);
	xfree(got);
}

static void parsed_eq(const char *id, int64_t net, const char *target) {
	int64_t n;
	struct strbuf t = STRBUF_INIT;
	assert_true(parse_msg_id(str_from(id), &n, &t, nullptr));
	assert_int_equal(n, net);
	assert_sb_eq(&t, target);
	strbuf_free(&t);
}

// Reference IDs produced by upstream Go soju
static void test_go_compat(void **state) {
	(void)state;
	id_eq(format_memory_msg_id(42, STR_LIT("#soju"), 1234567), "AFQFI3NvanUBh61L");
	id_eq(format_db_msg_id(7, STR_LIT("bob"), 99999), "AA4DYm9iA5-NBg");
	id_eq(format_db_msg_id(-3, STR_LIT(""), 0), "AAUAAwA");

	parsed_eq("AFQFI3NvanUBh61L", 42, "#soju");
	uint64_t seq;
	assert_true(parse_memory_msg_id(STR_LIT("AFQFI3NvanUBh61L"), &seq, nullptr));
	assert_int_equal(seq, 1234567);
	int64_t db;
	assert_false(parse_db_msg_id(STR_LIT("AFQFI3NvanUBh61L"), &db, nullptr));
	assert_true(parse_db_msg_id(STR_LIT("AA4DYm9iA5-NBg"), &db, nullptr));
	assert_int_equal(db, 99999);
	// An fs store ID made by upstream: recognized, but not a DB one
	parsed_eq("AAoFI2NoYW4C0B8GEoBA", 5, "#chan");
	int64_t n;
	struct strbuf body = STRBUF_INIT;
	assert_true(parse_msg_id_with(STR_LIT("AAoFI2NoYW4C0B8GEoBA"), MSG_ID_FS, &n, nullptr,
				      &body, nullptr));
	assert_true(body.len > 0);
	strbuf_free(&body);
	assert_false(parse_db_msg_id(STR_LIT("AAoFI2NoYW4C0B8GEoBA"), &db, nullptr));
	assert_false(parse_msg_id(STR_LIT("!!!"), &n, nullptr, nullptr));
	assert_false(parse_msg_id(STR_LIT(""), &n, nullptr, nullptr));
}

static void test_roundtrip(void **state) {
	(void)state;
	struct strbuf w = STRBUF_INIT;
	bare_write_uint(&w, 0);
	bare_write_uint(&w, 300);
	bare_write_int(&w, -1);
	bare_write_int(&w, INT64_MIN);
	bare_write_uint(&w, UINT64_MAX);
	bare_write_string(&w, STR_LIT("h\xC3\xA9llo"));
	struct str r = strbuf_str(&w);
	uint64_t u;
	int64_t i;
	assert_true(bare_read_uint(&r, &u, nullptr));
	assert_int_equal(u, 0);
	assert_true(bare_read_uint(&r, &u, nullptr));
	assert_int_equal(u, 300);
	assert_true(bare_read_int(&r, &i, nullptr));
	assert_int_equal(i, -1);
	assert_true(bare_read_int(&r, &i, nullptr));
	assert_true(i == INT64_MIN);
	assert_true(bare_read_uint(&r, &u, nullptr));
	assert_true(u == UINT64_MAX);
	struct strbuf s = STRBUF_INIT;
	assert_true(bare_read_string(&r, &s, nullptr));
	assert_sb_eq(&s, "h\xC3\xA9llo");
	struct err *e = nullptr;
	assert_false(bare_read_uint(&r, &u, &e));
	assert_string_equal(err_msg(e), "unexpected EOF");
	err_free(e);
	strbuf_free(&s);
	strbuf_free(&w);
}

// C-only: error messages
static void test_errors(void **state) {
	(void)state;
	int64_t n;
	struct err *e = nullptr;
	assert_false(parse_msg_id(STR_LIT("!!!"), &n, nullptr, &e));
	assert_string_equal(err_msg(e),
			    "invalid internal message ID: Invalid symbol 33, offset 0.");
	err_free(e);
	e = nullptr;
	assert_false(parse_msg_id(STR_LIT(""), &n, nullptr, &e));
	assert_string_equal(err_msg(e), "invalid internal message ID: unexpected EOF");
	err_free(e);
	e = nullptr;
	int64_t db;
	assert_false(parse_db_msg_id(STR_LIT("AFQFI3NvanUBh61L"), &db, &e));
	assert_string_equal(err_msg(e), "invalid internal message ID: got type 1, want 3");
	err_free(e);
}

T_MAIN(cmocka_unit_test(test_go_compat), cmocka_unit_test(test_roundtrip),
       cmocka_unit_test(test_errors))
