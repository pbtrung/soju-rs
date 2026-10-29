// Ported from soju-rs src/xirc/whox.rs tests
#include "testsupport.h"

#include "base/alloc.h"
#include "irc/whox.h"

static void opts_eq(const char *in, const char *fields, const char *token) {
	char *f, *t;
	parse_whox_options(str_from(in), &f, &t);
	assert_string_equal(f, fields);
	assert_string_equal(t, token);
	xfree(f);
	xfree(t);
}

static void test_options(void **state) {
	(void)state;
	opts_eq("", "", "");
	opts_eq("%tnA,42", "tna", "42");
	opts_eq("%na,42", "na", "");
}

static void test_roundtrip(void **state) {
	(void)state;
	struct whox_info info;
	whox_info_init(&info);
	whox_info_set(&info, 't', "42");
	whox_info_set(&info, 'c', "#a");
	whox_info_set(&info, 'u', "u");
	whox_info_set(&info, 'h', "::1");
	whox_info_set(&info, 's', "s");
	whox_info_set(&info, 'n', "n");
	whox_info_set(&info, 'f', "H");
	whox_info_set(&info, 'a', "acc");
	whox_info_set(&info, 'r', "real name");
	struct irc_msg *m = generate_whox_reply("tcuhsnfar", &info);
	assert_msg_str(m, "354 * 42 #a u 0::1 s n H acc :real name");
	struct whox_info parsed;
	struct err *e = nullptr;
	assert_true(parse_whox_reply(m, "tcuhsnfar", &parsed, &e));
	assert_string_equal(parsed.nickname, "n");
	assert_string_equal(parsed.hostname, "0::1");
	assert_string_equal(parsed.realname, "real name");
	whox_info_free(&parsed);
	irc_msg_free(m);

	m = generate_whox_reply("", &info);
	assert_msg_str(m, "352 * #a u 0::1 s n H :0 real name");
	irc_msg_free(m);
	whox_info_free(&info);
}

// C-only: error messages
static void test_reply_errors(void **state) {
	(void)state;
	struct whox_info info;
	struct err *e = nullptr;
	struct irc_msg *m = IRC_MSG("352", "*");
	assert_false(parse_whox_reply(m, "n", &info, &e));
	assert_string_equal(err_msg(e), "invalid WHOX reply \"352\"");
	err_free(e);
	irc_msg_free(m);
	e = nullptr;
	m = IRC_MSG("354", "*", "x");
	assert_false(parse_whox_reply(m, "nu", &info, &e));
	assert_string_equal(err_msg(e), "invalid RPL_WHOSPCRPL: missing value for field 'n'");
	err_free(e);
	irc_msg_free(m);
}

T_MAIN(cmocka_unit_test(test_options), cmocka_unit_test(test_roundtrip),
       cmocka_unit_test(test_reply_errors))
