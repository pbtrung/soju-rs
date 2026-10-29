// Ported from soju-rs src/irc/mod.rs tests
#include "testsupport.h"

#include <stdio.h>

#include "base/alloc.h"
#include "irc/message.h"

static void assert_formats(const struct irc_msg *m, const char *want) {
	char *s = irc_msg_to_string(m);
	assert_string_equal(s, want);
	xfree(s);
}

static struct irc_msg *parse(const char *line) {
	enum irc_parse_error e;
	struct irc_msg *m = irc_msg_parse(str_from(line), &e);
	assert_non_null(m);
	assert_int_equal(e, IRC_PARSE_OK);
	return m;
}

static void test_parse_basic(void **state) {
	(void)state;
	struct irc_msg *m = parse(":nick!user@host PRIVMSG #chan :hello world\r\n");
	assert_non_null(m->prefix);
	assert_string_equal(m->prefix->name, "nick");
	assert_string_equal(m->prefix->user, "user");
	assert_string_equal(m->prefix->host, "host");
	assert_string_equal(m->command, "PRIVMSG");
	assert_int_equal(m->params.len, 2);
	assert_string_equal(m->params.data[0], "#chan");
	assert_string_equal(m->params.data[1], "hello world");
	assert_formats(m, ":nick!user@host PRIVMSG #chan :hello world");
	irc_msg_free(m);
}

static void test_parse_tags(void **state) {
	(void)state;
	struct irc_msg *m = parse("@a=b\\sc;d;e=\\:\\\\x\\ :srv cmd  x  y");
	assert_string_equal(irc_tags_get(&m->tags, "a"), "b c");
	assert_string_equal(irc_tags_get(&m->tags, "d"), "");
	assert_string_equal(irc_tags_get(&m->tags, "e"), ";\\x");
	assert_string_equal(m->command, "CMD");
	assert_int_equal(m->params.len, 2);
	assert_string_equal(m->params.data[0], "x");
	assert_string_equal(m->params.data[1], "y");
	assert_formats(m, "@a=b\\sc;d;e=\\:\\\\x :srv CMD x y");
	irc_msg_free(m);
}

static void assert_parse_error(const char *line, enum irc_parse_error want) {
	enum irc_parse_error e;
	struct irc_msg *m = irc_msg_parse(str_from(line), &e);
	assert_null(m);
	assert_int_equal(e, want);
}

static void test_parse_errors(void **state) {
	(void)state;
	assert_parse_error("", IRC_PARSE_ZERO_LENGTH);
	assert_parse_error("\r\n", IRC_PARSE_ZERO_LENGTH);
	assert_parse_error("@a", IRC_PARSE_MISSING_DATA_AFTER_TAGS);
	assert_parse_error(":a", IRC_PARSE_MISSING_DATA_AFTER_PREFIX);
	assert_parse_error(":a  ", IRC_PARSE_MISSING_COMMAND);
	assert_parse_error("@a ", IRC_PARSE_MISSING_COMMAND);
	assert_string_equal(irc_parse_error_str(IRC_PARSE_MISSING_COMMAND),
			    "irc: missing message command");
}

static void test_format_trailing(void **state) {
	(void)state;
	struct irc_msg *m = IRC_MSG("CMD", "a", "");
	assert_formats(m, "CMD a :");
	irc_msg_free(m);
	m = IRC_MSG("CMD", ":x");
	assert_formats(m, "CMD ::x");
	irc_msg_free(m);
	m = IRC_MSG("CMD");
	assert_formats(m, "CMD");
	irc_msg_free(m);
	m = parse("CMD :");
	assert_int_equal(m->params.len, 1);
	assert_string_equal(m->params.data[0], "");
	irc_msg_free(m);
	m = parse("PING");
	assert_int_equal(m->params.len, 0);
	irc_msg_free(m);
}

static void test_empty_prefix_not_formatted(void **state) {
	(void)state;
	struct irc_msg *m = IRC_MSG("PING", "x");
	irc_msg_set_prefix(m, "", nullptr, nullptr);
	assert_formats(m, "PING x");
	irc_msg_free(m);
}

static void assert_prefix(const char *s, const char *name, const char *user, const char *host) {
	struct str n, u, h;
	irc_prefix_split(str_from(s), &n, &u, &h);
	assert_str_eq(n, name);
	assert_str_eq(u, user);
	assert_str_eq(h, host);
}

static void test_prefix_parse(void **state) {
	(void)state;
	assert_prefix("a", "a", "", "");
	assert_prefix("a@h", "a", "", "h");
	assert_prefix("a!u", "a", "u", "");
	assert_prefix("a!u@h!x", "a", "u", "h!x");
}

// Not in soju-rs: exercises the mutation API and the arena
static void test_mutation(void **state) {
	(void)state;
	struct irc_msg *m = parse("@x=1 :n!u@h privmsg #c :hi there");
	assert_string_equal(m->command, "PRIVMSG");
	const char *old = m->params.data[1];
	irc_msg_set_param(m, 1, "bye");
	// The old parameter stays valid until the message is freed
	assert_string_equal(old, "hi there");
	irc_msg_insert_param(m, 0, "first");
	irc_msg_add_param(m, "last one");
	irc_tags_set(&m->tags, "x", "2");
	irc_tags_set(&m->tags, "y", "a;b");
	assert_formats(m, "@x=2;y=a\\:b :n!u@h PRIVMSG first #c bye :last one");
	struct irc_msg *c = irc_msg_clone(m);
	irc_msg_free(m);
	assert_formats(c, "@x=2;y=a\\:b :n!u@h PRIVMSG first #c bye :last one");
	assert_true(irc_tags_remove(&c->tags, "x"));
	assert_false(irc_tags_remove(&c->tags, "x"));
	irc_msg_remove_param(c, 0);
	irc_msg_truncate_params(c, 1);
	irc_msg_clear_prefix(c);
	assert_formats(c, "@y=a\\:b PRIVMSG #c");
	assert_string_equal(irc_msg_param(c, 5), "");
	assert_string_equal(irc_msg_trailing(c), "#c");
	assert_string_equal(irc_msg_prefix_name(c), "");
	irc_msg_free(c);
}

static void test_tag_value_trailing_backslash(void **state) {
	(void)state;
	struct strbuf sb = STRBUF_INIT;
	irc_parse_tag_value(&sb, STR_LIT("a\\"));
	assert_string_equal(strbuf_cstr(&sb), "a");
	strbuf_reset(&sb);
	irc_parse_tag_value(&sb, STR_LIT("\\q\\r\\n"));
	assert_string_equal(strbuf_cstr(&sb), "q\r\n");
	strbuf_free(&sb);
}

static void test_parse_many_tags(void **state) {
	(void)state;
	// Same semantics with and without the key index
	for (int n = 4; n <= 100; n += 96) {
		struct strbuf s = STRBUF_INIT;
		for (int i = 0; i < n; i++) {
			strbuf_printf(&s, "%sk%d=%d", i > 0 ? ";" : "", i % (n / 2), i);
		}
		struct irc_tags tags = IRC_TAGS_INIT;
		irc_tags_parse(&tags, strbuf_str(&s));
		struct irc_tags want = IRC_TAGS_INIT;
		for (int i = 0; i < n; i++) {
			char k[16], v[16];
			snprintf(k, sizeof(k), "k%d", i % (n / 2));
			snprintf(v, sizeof(v), "%d", i);
			irc_tags_set(&want, k, v);
		}
		assert_int_equal(tags.v.len, want.v.len);
		for (size_t i = 0; i < tags.v.len; i++) {
			assert_string_equal(tags.v.data[i].key, want.v.data[i].key);
			assert_string_equal(tags.v.data[i].value, want.v.data[i].value);
		}
		assert_int_equal(tags.v.len, (size_t)(n / 2));
		char v0[16];
		snprintf(v0, sizeof(v0), "%d", n / 2);
		assert_string_equal(irc_tags_get(&tags, "k0"), v0);
		irc_tags_free(&tags);
		irc_tags_free(&want);
		strbuf_free(&s);
	}
}

// The index also applies when parsing into existing tags
static void test_parse_many_tags_existing(void **state) {
	(void)state;
	struct irc_tags tags = IRC_TAGS_INIT;
	irc_tags_set(&tags, "a", "1");
	irc_tags_parse(&tags, STR_LIT("a=2;b;c;d;e;f;g;h;i;j;k;l;m;n;o;p;q;r"));
	assert_int_equal(tags.v.len, 18);
	assert_string_equal(tags.v.data[0].key, "a");
	assert_string_equal(tags.v.data[0].value, "2");
	irc_tags_free(&tags);
}

// Commands are uppercased like Rust's str::to_uppercase (full mapping)
static void test_command_uppercase(void **state) {
	(void)state;
	struct irc_msg *m = parse("stra\xc3\x9f"
				  "e x");
	assert_string_equal(m->command, "STRASSE");
	irc_msg_free(m);
}

T_MAIN(cmocka_unit_test(test_parse_basic), cmocka_unit_test(test_parse_tags),
       cmocka_unit_test(test_parse_errors), cmocka_unit_test(test_format_trailing),
       cmocka_unit_test(test_empty_prefix_not_formatted), cmocka_unit_test(test_prefix_parse),
       cmocka_unit_test(test_mutation), cmocka_unit_test(test_tag_value_trailing_backslash),
       cmocka_unit_test(test_parse_many_tags), cmocka_unit_test(test_parse_many_tags_existing),
       cmocka_unit_test(test_command_uppercase))
