// Ported from soju-rs src/xirc/genmsg.rs tests
#include "testsupport.h"

#include <stdio.h>

#include "base/alloc.h"
#include "irc/genmsg.h"
#include "irc/numerics.h"

static void assert_fits(const irc_msg_vec *msgs) {
	for (size_t i = 0; i < msgs->len; i++) {
		char *s = irc_msg_to_string(msgs->data[i]);
		assert_true(strlen(s) <= MAX_MESSAGE_LENGTH);
		xfree(s);
	}
}

static void test_join_sorting_and_split(void **state) {
	(void)state;
	irc_msg_vec msgs = {0};
	generate_join(&msgs, (const char *[]){"#b", "#a", "#c"}, (const char *[]){"", "", "key"},
		      3);
	assert_int_equal(msgs.len, 1);
	assert_msg_str(msgs.data[0], "JOIN #c,#a,#b key");
	irc_msg_vec_free(&msgs);

	strvec channels = {0};
	const char *keys[100];
	char buf[32];
	for (int i = 0; i < 100; i++) {
		snprintf(buf, sizeof(buf), "#channel%03d", i);
		strvec_push(&channels, buf);
		keys[i] = "";
	}
	generate_join(&msgs, (const char *const *)channels.data, keys, 100);
	assert_true(msgs.len > 1);
	assert_fits(&msgs);
	size_t total = 0;
	for (size_t i = 0; i < msgs.len; i++) {
		total++;
		for (const char *p = msgs.data[i]->params.data[0]; *p != '\0'; p++) {
			total += *p == ',';
		}
	}
	assert_int_equal(total, 100);
	irc_msg_vec_free(&msgs);
	strvec_free(&channels);
}

static void test_sasl_chunks(void **state) {
	(void)state;
	irc_msg_vec msgs = {0};
	generate_sasl(&msgs, "", 0);
	assert_int_equal(msgs.len, 1);
	assert_string_equal(msgs.data[0]->params.data[0], "+");
	irc_msg_vec_free(&msgs);
	// 300 bytes -> 400 base64 chars -> one full chunk plus "+"
	char zeros[301] = {0};
	generate_sasl(&msgs, zeros, 300);
	assert_int_equal(msgs.len, 2);
	assert_string_equal(msgs.data[1]->params.data[0], "+");
	irc_msg_vec_free(&msgs);
	generate_sasl(&msgs, zeros, 301);
	assert_int_equal(msgs.len, 2);
	assert_string_not_equal(msgs.data[1]->params.data[0], "+");
	irc_msg_vec_free(&msgs);
}

static void test_isupport(void **state) {
	(void)state;
	struct isupport is;
	isupport_init(&is);
	char buf[16];
	for (int i = 0; i < 20; i++) {
		snprintf(buf, sizeof(buf), "T%02d", i);
		isupport_set(&is, buf, nullptr);
	}
	isupport_set(&is, "NETWORK", "a b");
	irc_msg_vec msgs = {0};
	generate_isupport(&msgs, &is);
	assert_int_equal(msgs.len, 2);
	assert_int_equal(msgs.data[0]->params.len, 15);
	assert_string_equal(msgs.data[0]->params.data[1], "NETWORK=a\\x20b");
	irc_msg_vec_free(&msgs);
	isupport_free(&is);
}

static void test_names(void **state) {
	(void)state;
	strvec members = {0};
	char buf[32];
	for (int i = 0; i < 200; i++) {
		snprintf(buf, sizeof(buf), "@nick%d", i);
		strvec_push(&members, buf);
	}
	irc_msg_vec msgs = {0};
	generate_names_reply(&msgs, "#chan", CHANNEL_PUBLIC, (const char *const *)members.data,
			     200);
	assert_string_equal(vec_last(&msgs)->command, RPL_ENDOFNAMES);
	assert_fits(&msgs);
	irc_msg_vec_free(&msgs);
	strvec_free(&members);
}

static void test_monitor(void **state) {
	(void)state;
	strvec targets = {0};
	char buf[32];
	for (int i = 0; i < 200; i++) {
		snprintf(buf, sizeof(buf), "nick%d", i);
		strvec_push(&targets, buf);
	}
	irc_msg_vec msgs = {0};
	generate_monitor(&msgs, "+", (const char *const *)targets.data, 200);
	assert_true(msgs.len > 1);
	assert_fits(&msgs);
	irc_msg_vec_free(&msgs);
	strvec_free(&targets);
}

// C-only
static void test_motd(void **state) {
	(void)state;
	irc_msg_vec msgs = {0};
	generate_motd(&msgs, STR_LIT("a\nb"));
	assert_int_equal(msgs.len, 4);
	assert_msg_str(msgs.data[1], "372 * a");
	assert_msg_str(msgs.data[3], "376 * :End of /MOTD command.");
	irc_msg_vec_free(&msgs);
}

T_MAIN(cmocka_unit_test(test_join_sorting_and_split), cmocka_unit_test(test_sasl_chunks),
       cmocka_unit_test(test_isupport), cmocka_unit_test(test_names),
       cmocka_unit_test(test_monitor), cmocka_unit_test(test_motd))
