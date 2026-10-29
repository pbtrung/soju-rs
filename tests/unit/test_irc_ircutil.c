// Ported from soju-rs src/ircutil.rs tests
#include "testsupport.h"

#include "base/alloc.h"
#include "irc/ircutil.h"

static void test_user_modes_non_ascii(void **state) {
	(void)state;
	struct user_modes m = {0};
	assert_true(user_modes_apply(&m, STR_LIT("+iw\xC3\xA9"), nullptr));
	// Non-ASCII mode bytes are ignored, rather than panicking on removal
	assert_true(user_modes_apply(&m, STR_LIT("-\xC3\xA9w"), nullptr));
	assert_sb_eq(&m.s, "i");
	user_modes_free(&m);
}

static void test_highlight(void **state) {
	(void)state;
	static const struct {
		const char *name, *text;
		bool hl;
	} cases[] = {
		{"noContains", "hi there Soju User!", false},
		{"standalone", "SojuUser", true},
		{"middle", "hi there SojuUser!", true},
		{"start", "SojuUser: how are you doing?", true},
		{"end", "maybe ask SojuUser", true},
		{"inWord", "but OtherSojuUserSan is a different nick", false},
		{"startWord", "and OtherSojuUser is another different nick", false},
		{"endWord", "and SojuUserSan is yet a different nick", false},
		{"underscore", "and SojuUser_san has nothing to do with me", false},
		{"zeroWidthSpace", "writing S\xE2\x80\x8BojuUser shouldn't trigger a highlight",
		 false},
		{"url", "https://SojuUser.example", false},
		{"startURL", "https://SojuUser.example is a nice website", false},
		{"endURL", "check out my website: https://SojuUser.example", false},
		{"parenthesizedURL", "see my website (https://SojuUser.example)", false},
		{"afterURL", "see https://SojuUser.example (cc SojuUser)", true},
	};
	for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
		if (is_highlight(str_from(cases[i].text), STR_LIT("SojuUser")) != cases[i].hl) {
			fail_msg("%s", cases[i].name);
		}
	}
}

static void assert_split(const char *input, const char *const *want, size_t n) {
	strvec words = {0};
	struct err *e = nullptr;
	assert_true(split_words(str_from(input), &words, &e));
	assert_int_equal(words.len, n);
	for (size_t i = 0; i < n; i++) {
		assert_string_equal(words.data[i], want[i]);
	}
	strvec_free(&words);
}

static void test_split(void **state) {
	(void)state;
	assert_split("  ch 'up' #soju    'relay'-det\"ache\"d  message  ",
		     (const char *[]){"ch", "up", "#soju", "relay-detached", "message"}, 5);
	assert_split("net update \\\"free\\\"node -pass 'political \"stance\" desu!' -realname '' "
		     "-nick lee",
		     (const char *[]){"net", "update", "\"free\"node", "-pass",
				      "political \"stance\" desu!", "-realname", "", "-nick",
				      "lee"},
		     9);
	assert_split("Omedeto,\\ Yui! ''", (const char *[]){"Omedeto, Yui!", ""}, 2);
	strvec words = {0};
	struct err *e = nullptr;
	assert_false(split_words(STR_LIT("end of 'file"), &words, &e));
	assert_string_equal(err_msg(e), "unterminated quoted string");
	err_free(e);
	e = nullptr;
	assert_false(split_words(STR_LIT("end of backquote \\"), &words, &e));
	assert_string_equal(err_msg(e), "unterminated backslash sequence");
	err_free(e);
	assert_int_equal(words.len, 0);
}

static void test_user_modes(void **state) {
	(void)state;
	struct user_modes m = {0};
	assert_true(user_modes_apply(&m, STR_LIT("+iw-i+o"), nullptr));
	assert_sb_eq(&m.s, "wo");
	struct err *e = nullptr;
	assert_false(user_modes_apply(&m, STR_LIT("x"), &e));
	assert_string_equal(err_msg(e), "malformed modestring \"x\": missing plus/minus");
	err_free(e);
	user_modes_free(&m);
}

static void free_ms(void *p) {
	membership_set_free(p);
	xfree(p);
}

static void test_channel_modes(void **state) {
	(void)state;
	size_t n_avail;
	const struct membership *avail = std_memberships(&n_avail);
	struct channel_mode_types chmodes = {0};
	std_channel_modes(&chmodes);
	struct cmmap members;
	cmmap_init(&members, CASEMAPPING_RFC1459, free_ms);
	cmmap_set(&members, "Alice", xcalloc(1, sizeof(struct membership_set)));
	struct channel_modes modes = {0};
	const char *args[] = {"alice", "*!*@x", "key", "10"};
	assert_true(apply_channel_modes(&modes, &members, avail, n_avail, &chmodes,
					STR_LIT("+obkln"), args, 4, nullptr));
	struct membership_set *ms = cmmap_get(&members, "ALICE");
	assert_int_equal(ms->v.len, 1);
	assert_true(membership_eq(ms->v.data[0], avail[2]));
	struct strbuf s = STRBUF_INIT;
	strvec p = {0};
	format_channel_modes(&modes, &s, &p);
	assert_sb_eq(&s, "+kln");
	assert_int_equal(p.len, 2);
	assert_string_equal(p.data[0], "key");
	assert_string_equal(p.data[1], "10");
	strvec_free(&p);
	strbuf_reset(&s);
	assert_true(apply_channel_modes(&modes, &members, avail, n_avail, &chmodes, STR_LIT("-lk"),
					(const char *[]){"x"}, 1, nullptr));
	format_channel_modes(&modes, &s, &p);
	assert_sb_eq(&s, "+n");
	strvec_free(&p);
	strbuf_free(&s);
	struct err *e = nullptr;
	assert_false(apply_channel_modes(nullptr, &members, avail, n_avail, &chmodes, STR_LIT("+o"),
					 nullptr, 0, &e));
	assert_string_equal(err_msg(e),
			    "malformed modestring \"+o\": missing mode argument for +o");
	err_free(e);
	channel_modes_free(&modes);
	channel_mode_types_free(&chmodes);
	cmmap_free(&members);
}

static void test_member_prefix(void **state) {
	(void)state;
	size_t n_avail;
	const struct membership *avail = std_memberships(&n_avail);
	struct membership_set ms = {0};
	vec_push(&ms.v, avail[2]);
	vec_push(&ms.v, avail[4]);
	struct strbuf s = STRBUF_INIT;
	format_member_prefix(&s, &ms, true);
	assert_sb_eq(&s, "@+");
	strbuf_reset(&s);
	format_member_prefix(&s, &ms, false);
	assert_sb_eq(&s, "@");
	strbuf_reset(&s);
	strip_member_prefixes(&s, STR_LIT("H@+"), avail, n_avail);
	assert_sb_eq(&s, "H");
	strbuf_free(&s);
	membership_set_free(&ms);
}

// C-only
static void test_misc(void **state) {
	(void)state;
	assert_true(is_numeric("001"));
	assert_false(is_numeric("PRIVMSG"));
	struct irc_msg *m = IRC_MSG("PASS", "secret");
	char *s = redact_for_log(m);
	assert_string_equal(s, "PASS <redacted>");
	xfree(s);
	irc_msg_free(m);
	struct timestamp t;
	assert_true(parse_chathistory_bound(STR_LIT("timestamp=2024-01-02T03:04:05Z"), &t));
	assert_false(parse_chathistory_bound(STR_LIT("msgid=x"), &t));
	struct irc_tags tags = IRC_TAGS_INIT, out = IRC_TAGS_INIT;
	irc_tags_parse(&tags, STR_LIT("+a=1;b=2;+c"));
	copy_client_tags(&out, &tags);
	assert_int_equal(out.v.len, 2);
	irc_tags_free(&tags);
	irc_tags_free(&out);
}

T_MAIN(cmocka_unit_test(test_user_modes_non_ascii), cmocka_unit_test(test_highlight),
       cmocka_unit_test(test_split), cmocka_unit_test(test_user_modes),
       cmocka_unit_test(test_channel_modes), cmocka_unit_test(test_member_prefix),
       cmocka_unit_test(test_misc))
