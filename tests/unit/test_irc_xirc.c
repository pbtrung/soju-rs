// Ported from soju-rs src/xirc/mod.rs tests (server_time is in
// test_base_timeutil.c)
#include "testsupport.h"

#include "base/alloc.h"
#include "irc/xirc.h"

static void test_membership_set(void **state) {
	(void)state;
	const struct membership avail[] = {{'o', '@'}, {'h', '%'}, {'v', '+'}};
	struct membership_set ms = {0};
	membership_set_add(&ms, avail, 3, avail[2]);
	membership_set_add(&ms, avail, 3, avail[0]);
	membership_set_add(&ms, avail, 3, avail[0]);
	membership_set_add(&ms, avail, 3, avail[1]);
	assert_int_equal(ms.v.len, 3);
	for (size_t i = 0; i < 3; i++) {
		assert_true(membership_eq(ms.v.data[i], avail[i]));
	}
	membership_set_remove(&ms, avail[1]);
	assert_int_equal(ms.v.len, 2);
	assert_true(membership_eq(ms.v.data[0], avail[0]));
	assert_true(membership_eq(ms.v.data[1], avail[2]));
	membership_set_free(&ms);
}

static void test_isupport_value(void **state) {
	(void)state;
	struct strbuf sb = STRBUF_INIT;
	decode_isupport_value(&sb, STR_LIT("a\\x20b\\x5Cc"));
	assert_sb_eq(&sb, "a b\\c");
	strbuf_reset(&sb);
	encode_isupport_value(&sb, STR_LIT("a b\\c"));
	assert_sb_eq(&sb, "a\\x20b\\x5Cc");
	strbuf_free(&sb);
}

// C-only
static void test_channel_status(void **state) {
	(void)state;
	enum channel_status st;
	struct err *e = nullptr;
	assert_true(channel_status_parse(STR_LIT("@"), &st, &e));
	assert_int_equal(st, CHANNEL_SECRET);
	assert_false(channel_status_parse(STR_LIT("=="), &st, &e));
	assert_string_equal(err_msg(e), "invalid channel status \"==\": more than one character");
	err_free(e);
	e = nullptr;
	assert_false(channel_status_parse(STR_LIT("x"), &st, &e));
	assert_string_equal(err_msg(e), "invalid channel status \"x\": unknown status");
	err_free(e);
}

static void test_cap_registry(void **state) {
	(void)state;
	struct cap_registry r;
	cap_registry_init(&r);
	cap_registry_set_available(&r, "sasl", "PLAIN");
	cap_registry_set_enabled(&r, "sasl", true);
	assert_true(cap_registry_is_enabled(&r, "sasl"));
	assert_string_equal(cap_registry_value(&r, "sasl"), "PLAIN");
	cap_registry_del(&r, "sasl");
	assert_false(cap_registry_is_available(&r, "sasl"));
	assert_false(cap_registry_is_enabled(&r, "sasl"));
	cap_registry_free(&r);
}

T_MAIN(cmocka_unit_test(test_membership_set), cmocka_unit_test(test_isupport_value),
       cmocka_unit_test(test_channel_status), cmocka_unit_test(test_cap_registry))
