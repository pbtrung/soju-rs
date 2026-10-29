// Ported from soju-rs src/flags.rs tests
#include "testsupport.h"

#include "config/flags.h"

static void test_parse(void **state) {
	(void)state;
	struct flag_set fs;
	flag_set_init(&fs);
	flag_set_add(&fs, "addr", FLAG_STRING);
	flag_set_add(&fs, "ignore-limit", FLAG_BOOL);
	flag_set_add(&fs, "enabled", FLAG_BOOL_VALUE);
	flag_set_add(&fs, "bits", FLAG_INT);
	flag_set_add(&fs, "connect-command", FLAG_STRING);
	const char *args[] = {"-addr",
			      "irc.example.org",
			      "--ignore-limit",
			      "-enabled=false",
			      "-bits=4096",
			      "-connect-command",
			      "a",
			      "-connect-command",
			      "b",
			      "rest",
			      "-addr"};
	struct flags_parsed p;
	assert_true(flag_set_parse(&fs, args, 11, &p, nullptr));
	assert_string_equal(flags_get(&p, "addr"), "irc.example.org");
	bool b;
	assert_true(flags_get_bool(&p, "ignore-limit", &b) && b);
	assert_true(flags_get_bool(&p, "enabled", &b) && !b);
	long long n;
	assert_true(flags_get_int(&p, "bits", &n));
	assert_int_equal(n, 4096);
	const strvec *cc = flags_get_all(&p, "connect-command");
	assert_int_equal(cc->len, 2);
	assert_string_equal(cc->data[0], "a");
	assert_string_equal(cc->data[1], "b");
	assert_int_equal(p.args.len, 2);
	assert_string_equal(p.args.data[0], "rest");
	assert_string_equal(p.args.data[1], "-addr");
	flags_parsed_free(&p);

	assert_true(flag_set_parse(&fs, (const char *[]){"--", "-addr"}, 2, &p, nullptr));
	assert_int_equal(p.args.len, 1);
	assert_string_equal(p.args.data[0], "-addr");
	flags_parsed_free(&p);
	assert_true(flag_set_parse(&fs, (const char *[]){"-", "x"}, 2, &p, nullptr));
	assert_int_equal(p.args.len, 2);
	assert_string_equal(p.args.data[0], "-");
	flags_parsed_free(&p);
	flag_set_free(&fs);
}

static void parse_err(const struct flag_set *fs, const char *const *args, size_t n,
		      const char *want, int code) {
	struct flags_parsed p;
	struct err *e = nullptr;
	assert_false(flag_set_parse(fs, args, n, &p, &e));
	assert_string_equal(err_msg(e), want);
	assert_int_equal(e->code, code);
	err_free(e);
}

static void test_errors(void **state) {
	(void)state;
	struct flag_set fs;
	flag_set_init(&fs);
	flag_set_add(&fs, "addr", FLAG_STRING);
	flag_set_add(&fs, "enabled", FLAG_BOOL_VALUE);
	flag_set_add(&fs, "bits", FLAG_INT);
	parse_err(&fs, (const char *[]){"-nope"}, 1, "flag provided but not defined: -nope", 0);
	parse_err(&fs, (const char *[]){"-addr"}, 1, "flag needs an argument: -addr", 0);
	parse_err(&fs, (const char *[]){"---x"}, 1, "bad flag syntax: ---x", 0);
	parse_err(
		&fs, (const char *[]){"-enabled", "maybe"}, 2,
		"invalid value \"maybe\" for flag -enabled: strconv.ParseBool: parsing \"maybe\": "
		"invalid syntax",
		0);
	parse_err(&fs, (const char *[]){"-bits", "x"}, 2,
		  "invalid value \"x\" for flag -bits: parse error", 0);
	parse_err(&fs, (const char *[]){"-help"}, 1, "flag: help requested", FLAG_ERR_HELP);
	// C-only: values parsed before an error are freed
	parse_err(&fs, (const char *[]){"-addr", "a", "-nope"}, 3,
		  "flag provided but not defined: -nope", 0);
	flag_set_free(&fs);
}

T_MAIN(cmocka_unit_test(test_parse), cmocka_unit_test(test_errors))
