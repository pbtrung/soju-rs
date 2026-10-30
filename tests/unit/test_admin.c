// Ported from src/bin/sojuctl.rs: argument quoting and offline validation.
#include "testsupport.h"
#include "base/alloc.h"
#include "irc/ircutil.h"
#include "server/admin.h"

static void test_quoting(void **state) {
	(void)state;
	const char *words[] = {"user", "create", "-password", "a \"b\"\\"};
	AUTO_STRBUF struct strbuf out = STRBUF_INIT;
	assert_true(admin_quote_words(words, 4, &out, nullptr));
	assert_sb_eq(&out, "\"user\" \"create\" \"-password\" \"a \\\"b\\\"\\\\\"");
	strvec parsed = {0};
	assert_true(split_words(strbuf_str(&out), &parsed, nullptr));
	assert_int_equal(parsed.len, 4);
	for (size_t i = 0; i < parsed.len; i++) {
		assert_string_equal(parsed.data[i], words[i]);
	}
	strvec_free(&parsed);
}
static void test_quoting_roundtrip(void **state) {
	(void)state;
	const char *words[] = {"a\tb",          "\a\x1b[1m", "'single' \"double\"",
			       "back\\slash\\", "",          "é ✓"};
	for (size_t i = 0; i < sizeof(words) / sizeof(words[0]); i++) {
		AUTO_STRBUF struct strbuf out = STRBUF_INIT;
		assert_true(admin_quote_words(words + i, 1, &out, nullptr));
		struct irc_msg *msg = IRC_MSG("BOUNCERSERV", strbuf_cstr(&out));
		AUTOFREE char *wire = irc_msg_to_string(msg);
		irc_msg_free(msg);
		msg = irc_msg_parse(str_from(wire), nullptr);
		assert_non_null(msg);
		strvec parsed = {0};
		assert_true(split_words(str_from(irc_msg_param(msg, 0)), &parsed, nullptr));
		assert_int_equal(parsed.len, 1);
		assert_string_equal(parsed.data[0], words[i]);
		strvec_free(&parsed);
		irc_msg_free(msg);
	}
	AUTO_STRBUF struct strbuf out = STRBUF_INIT;
	const char *bad[] = {"a\nb"};
	assert_false(admin_quote_words(bad, 1, &out, nullptr));
}
static void test_turso_bootstrap_validates_before_connecting(void **state) {
	(void)state;
	struct config cfg;
	config_defaults(&cfg);
	struct err *error = nullptr;
	assert_false(admin_create_turso_admin(&cfg, nullptr, 0, &error));
	assert_non_null(strstr(err_msg(error), "message-store turso"));
	err_free(error);
	error = nullptr;
	cfg.has_turso = true;
	assert_false(admin_create_turso_admin(&cfg, nullptr, 0, &error));
	assert_non_null(strstr(err_msg(error), "-username"));
	err_free(error);
	error = nullptr;
	const char *missing[] = {"-username", "alice"};
	assert_false(admin_create_turso_admin(&cfg, missing, 2, &error));
	assert_non_null(strstr(err_msg(error), "-password"));
	err_free(error);
	error = nullptr;
	const char *bad[] = {"-username", "alice\r\nJOIN #channel", "-password", "secret"};
	assert_false(admin_create_turso_admin(&cfg, bad, 4, &error));
	assert_non_null(strstr(err_msg(error), "CR, LF or NUL"));
	err_free(error);
	cfg.has_turso = false;
	config_free(&cfg);
}
T_MAIN(cmocka_unit_test(test_quoting), cmocka_unit_test(test_quoting_roundtrip),
       cmocka_unit_test(test_turso_bootstrap_validates_before_connecting))
