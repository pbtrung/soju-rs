// Ported from soju-rs src/xirc/multiline.rs tests
#include "testsupport.h"

#include "base/alloc.h"
#include "irc/multiline.h"

static const struct multiline_limits LIMITS = {.max_bytes = 4096, .max_lines = 24};

static void test_limits(void **state) {
	(void)state;
	struct multiline_limits l = multiline_limits_parse(STR_LIT("max-bytes=4096,max-lines=24"));
	assert_int_equal(l.max_bytes, LIMITS.max_bytes);
	assert_int_equal(l.max_lines, LIMITS.max_lines);
	l = multiline_limits_parse(STR_LIT("max-bytes=1000000"));
	assert_int_equal(l.max_bytes, MAX_MULTILINE_BYTES);
	assert_int_equal(l.max_lines, MAX_MULTILINE_LINES);
}

static void test_split(void **state) {
	(void)state;
	multiline_line_vec l = {0};
	split_multiline(&l, STR_LIT("a\n\nb"), 100);
	assert_int_equal(l.len, 3);
	assert_str_eq(l.data[0].text, "a");
	assert_str_eq(l.data[1].text, "");
	assert_str_eq(l.data[2].text, "b");
	for (size_t i = 0; i < 3; i++) {
		assert_false(l.data[i].concat);
	}
	vec_free(&l);

	struct strbuf longs = STRBUF_INIT;
	for (int i = 0; i < 50; i++) {
		strbuf_append_cstr(&longs, "\xC3\xA9");
	}
	split_multiline(&l, strbuf_str(&longs), 65);
	assert_int_equal(l.len, 2);
	assert_int_equal(l.data[0].text.len, 64);
	assert_true(!l.data[0].concat && l.data[1].concat);
	assert_int_equal(l.data[0].text.len + l.data[1].text.len, longs.len);
	assert_true(l.data[1].text.p == l.data[0].text.p + 64);
	vec_free(&l);
	strbuf_free(&longs);
}

static struct irc_msg *parse(const char *s) {
	struct irc_msg *m = irc_msg_parse(str_from(s), nullptr);
	assert_non_null(m);
	return m;
}

static void test_assemble(void **state) {
	(void)state;
	struct multiline_batch b;
	multiline_batch_init(&b, parse("@label=x;+reply=1 :a BATCH +r draft/multiline #c"));
	struct irc_msg *m1 = IRC_MSG("PRIVMSG", "#c", "hello");
	struct irc_msg *m2 = IRC_MSG("PRIVMSG", "#c", " world");
	irc_tags_set(&m2->tags, MULTILINE_CONCAT_TAG, "");
	struct irc_msg *m3 = IRC_MSG("PRIVMSG", "#c", "");
	struct irc_msg *m4 = IRC_MSG("PRIVMSG", "#c", "bye");
	multiline_batch_push(&b, m1, &LIMITS);
	multiline_batch_push(&b, m2, &LIMITS);
	multiline_batch_push(&b, m3, &LIMITS);
	multiline_batch_push(&b, m4, &LIMITS);
	irc_msg_free(m1);
	irc_msg_free(m2);
	irc_msg_free(m3);
	irc_msg_free(m4);
	enum multiline_error err;
	struct irc_msg *m = multiline_batch_finish(&b, &err);
	assert_non_null(m);
	assert_int_equal(m->params.len, 2);
	assert_string_equal(m->params.data[0], "#c");
	assert_string_equal(m->params.data[1], "hello world\n\nbye");
	assert_string_equal(irc_tags_get(&m->tags, "label"), "x");
	assert_string_equal(irc_msg_prefix_name(m), "a");

	irc_msg_vec batch = {0};
	generate_multiline_batch(&batch, m, "1", 100);
	const char *want[] = {
		"@label=x;+reply=1 :a BATCH +1 draft/multiline #c",
		"@batch=1 :a PRIVMSG #c :hello world",
		"@batch=1 :a PRIVMSG #c :",
		"@batch=1 :a PRIVMSG #c bye",
		"BATCH -1",
	};
	assert_int_equal(batch.len, 5);
	for (size_t i = 0; i < 5; i++) {
		assert_msg_str(batch.data[i], want[i]);
	}
	irc_msg_vec_free(&batch);

	irc_tags_set(&m->tags, "msgid", "id");
	irc_msg_vec l = {0};
	generate_multiline_fallback(&l, m, 100);
	assert_int_equal(l.len, 2);
	assert_string_equal(irc_tags_get(&l.data[0]->tags, "msgid"), "id");
	assert_null(irc_tags_get(&l.data[1]->tags, "msgid"));
	assert_string_equal(l.data[1]->params.data[0], "#c");
	assert_string_equal(l.data[1]->params.data[1], "bye");
	irc_msg_vec_free(&l);
	irc_msg_free(m);
}

static enum multiline_error finish_err(struct multiline_batch *b) {
	enum multiline_error err;
	struct irc_msg *m = multiline_batch_finish(b, &err);
	assert_null(m);
	return err;
}

static void push(struct multiline_batch *b, const char *cmd, const char *target, const char *text) {
	struct irc_msg *m = IRC_MSG(cmd, target, text);
	multiline_batch_push(b, m, &LIMITS);
	irc_msg_free(m);
}

static void test_errors(void **state) {
	(void)state;
	struct irc_msg *opener = IRC_MSG("BATCH", "+r", "draft/multiline", "#c");
	struct multiline_batch b;
	multiline_batch_init(&b, irc_msg_clone(opener));
	push(&b, "PRIVMSG", "#d", "hello");
	assert_int_equal(finish_err(&b), MULTILINE_INVALID_TARGET);

	multiline_batch_init(&b, irc_msg_clone(opener));
	push(&b, "PRIVMSG", "#c", "a");
	push(&b, "NOTICE", "#c", "b");
	assert_int_equal(finish_err(&b), MULTILINE_INVALID);

	multiline_batch_init(&b, irc_msg_clone(opener));
	for (int i = 0; i < 25; i++) {
		push(&b, "PRIVMSG", "#c", "a");
	}
	assert_int_equal(finish_err(&b), MULTILINE_MAX_LINES);

	multiline_batch_init(&b, irc_msg_clone(opener));
	char *big = xmalloc(4098);
	memset(big, 'a', 4097);
	big[4097] = '\0';
	push(&b, "PRIVMSG", "#c", big);
	xfree(big);
	assert_int_equal(finish_err(&b), MULTILINE_MAX_BYTES);

	multiline_batch_init(&b, opener);
	assert_int_equal(finish_err(&b), MULTILINE_INVALID);
}

T_MAIN(cmocka_unit_test(test_limits), cmocka_unit_test(test_split), cmocka_unit_test(test_assemble),
       cmocka_unit_test(test_errors))
