#include "testsupport.h"
#include "base/alloc.h"
#include "service/service.h"

static void test_lookup(void **state) {
	(void)state;
	size_t used = 0;
	struct err *err = nullptr;
	const struct service_command *cmd =
		service_lookup((const char *const[]){"net", "crea", "-addr", "x"}, 4, &used, &err);
	assert_non_null(cmd);
	assert_string_equal(cmd->name, "create");
	assert_int_equal(used, 2);
	cmd = service_lookup((const char *const[]){"n", "st"}, 2, &used, &err);
	assert_non_null(cmd);
	assert_string_equal(cmd->name, "status");
	assert_null(service_lookup((const char *const[]){"s"}, 1, &used, &err));
	assert_non_null(strstr(err_msg(err), "ambiguous"));
	err_free(err);
	err = nullptr;
	assert_null(service_lookup((const char *const[]){"nope"}, 1, &used, &err));
	err_free(err);
	err = nullptr;
	cmd = service_lookup((const char *const[]){"channel"}, 1, &used, &err);
	assert_non_null(cmd);
	assert_string_equal(cmd->name, "channel");
	assert_int_equal(used, 1);
	assert_null(service_lookup(nullptr, 0, &used, &err));
	err_free(err);
}
static void test_help_listing(void **state) {
	(void)state;
	strvec l = {0};
	service_help(false, false, &l);
	assert_true(strvec_index(&l, "network create") >= 0);
	assert_int_equal(strvec_index(&l, "server status"), -1);
	assert_int_equal(strvec_index(&l, "user create"), -1);
	strvec_free(&l);
	service_help(true, true, &l);
	assert_true(strvec_index(&l, "user create") >= 0);
	assert_true(strvec_index(&l, "server status") >= 0);
	assert_int_equal(strvec_index(&l, "network create"), -1);
	strvec_free(&l);
}
static void test_durations(void **state) {
	(void)state;
	uint64_t ns = 0;
	const char *const good[] = {"0", "300s", "1h30m", "2.5h", "2562047h"};
	const uint64_t want[] = {0, 300000000000ULL, 5400000000000ULL, 9000000000000ULL,
				 9223369200000000000ULL};
	for (size_t i = 0; i < 5; i++) {
		assert_true(service_parse_duration(good[i], &ns));
		assert_int_equal(ns, want[i]);
	}
	const char *const bad[] = {"-1s", "10", "x",   "9999999999999999h", "1.2.3s", "",
				   "+",   ".s", "1e3s"};
	for (size_t i = 0; i < 9; i++) {
		assert_false(service_parse_duration(bad[i], &ns));
	}
	char huge[402];
	memset(huge, '9', 400);
	huge[400] = 's';
	huge[401] = '\0';
	assert_false(service_parse_duration(huge, &ns));
	const char *const roundtrip[] = {"0s", "5m0s", "22h30m0s", "1h0m1.25s", "0.000000001s"};
	for (size_t i = 0; i < 5; i++) {
		assert_true(service_parse_duration(roundtrip[i], &ns));
		AUTOFREE char *out = service_format_duration(ns);
		assert_string_equal(out, roundtrip[i]);
	}
	AUTOFREE char *a = service_redact_command("PRIVMSG NickServ :IDENTIFY hunter2");
	assert_string_equal(a, "PRIVMSG NickServ (parameters hidden)");
	AUTOFREE char *b = service_redact_command("OPER x");
	assert_string_equal(b, "OPER (parameters hidden)");
	AUTOFREE char *d = service_redact_command("AWAY");
	assert_string_equal(d, "AWAY");
}
T_MAIN(cmocka_unit_test(test_lookup), cmocka_unit_test(test_help_listing),
       cmocka_unit_test(test_durations))
