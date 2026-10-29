// Rust tests/server.rs scenarios through real client and upstream sockets.
#include "server_fixture.h"
#include "base/alloc.h"
#include "service/service.h"
#include <unistd.h>

static void test_broadcast(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	(void)up;
	struct test_peer *a = fixture_connect(&e), *b = fixture_connect(&e);
	fixture_register(&e, a, true, "");
	fixture_register(&e, b, false, "");
	struct irc_msg *notice = IRC_MSG("NOTICE", "*", "broadcast");
	server_set_prefix(&e.srv, notice);
	user_broadcast_take(&e.srv, notice);
	struct irc_msg *m = peer_until(a, "NOTICE");
	assert_string_equal(irc_msg_param(m, 1), "broadcast");
	irc_msg_free(m);
	m = peer_until(b, "NOTICE");
	assert_string_equal(irc_msg_param(m, 1), "broadcast");
	irc_msg_free(m);
	fixture_finish(&e);
}

static void test_chat_history_params(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	peer_send(up, "@time=2023-05-23T06:00:00.000Z :Foo!u@h PRIVMSG test :msg 0");
	peer_send(up, "@time=2023-05-23T06:00:01.000Z :Foo!u@h PRIVMSG test :msg 1");
	peer_send(up, "@time=2023-05-23T06:00:02.000Z :Foo!u@h PRIVMSG test :msg 2");
	peer_sync(up);
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, true, "batch server-time draft/chathistory");
	const char *bad[] = {"-1", "1001", "abc"};
	for (size_t i = 0; i < 3; i++) {
		peer_send_take(dc, IRC_MSG("CHATHISTORY", "LATEST", "foo", "*", bad[i]));
		struct irc_msg *m = peer_until(dc, "FAIL");
		assert_string_equal(irc_msg_param(m, 3), bad[i]);
		assert_string_equal(irc_msg_param(m, 4), "Invalid limit");
		irc_msg_free(m);
	}
	peer_send(dc, "CHATHISTORY LATEST FOO * 2");
	irc_msg_vec history = {0};
	peer_roundtrip(dc, &history);
	assert_int_equal(history.len, 2);
	assert_string_equal(irc_msg_param(history.data[0], 1), "msg 1");
	assert_string_equal(irc_msg_param(history.data[1], 1), "msg 2");
	assert_non_null(irc_tags_get(&history.data[0]->tags, "batch"));
	assert_non_null(irc_tags_get(&history.data[0]->tags, "time"));
	irc_msg_vec_free(&history);
	peer_send(dc, "CHATHISTORY LATEST foo * 0");
	peer_roundtrip(dc, &history);
	assert_int_equal(history.len, 0);
	fixture_finish(&e);
}

static void test_read_marker(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	(void)up;
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, true, "draft/read-marker");
	peer_send(dc, "MARKREAD #rm");
	struct irc_msg *m = peer_until(dc, "MARKREAD");
	assert_string_equal(irc_msg_param(m, 1), "*");
	irc_msg_free(m);
	peer_send(dc, "MARKREAD #RM timestamp=2024-01-01T00:00:00.000Z");
	m = peer_until(dc, "MARKREAD");
	assert_string_equal(irc_msg_param(m, 1), "timestamp=2024-01-01T00:00:00.000Z");
	irc_msg_free(m);
	peer_send(dc, "MARKREAD #rm timestamp=2023-01-01T00:00:00.000Z");
	m = peer_until(dc, "MARKREAD");
	assert_string_equal(irc_msg_param(m, 1), "timestamp=2024-01-01T00:00:00.000Z");
	irc_msg_free(m);
	fixture_finish(&e);
}

static void test_labeled_response_ack(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	(void)up;
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, false, "labeled-response batch");
	peer_send(dc, "@label=z PONG foo");
	struct irc_msg *m = peer_until(dc, "ACK");
	assert_string_equal(irc_tags_get(&m->tags, "label"), "z");
	irc_msg_free(m);
	peer_send(dc, "@label=e CHATHISTORY WHAT #x");
	m = peer_until(dc, "FAIL");
	assert_string_equal(irc_tags_get(&m->tags, "label"), "e");
	assert_string_equal(irc_msg_param(m, 1), "INVALID_PARAMS");
	irc_msg_free(m);
	fixture_finish(&e);
}

static void test_monitor_invalid_targets(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "MONITOR=100");
	(void)up;
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, true, "");
	peer_send(dc, "MONITOR + a,,b");
	peer_send(dc, "MONITOR + :c d");
	peer_send(dc, "MONITOR L");
	struct irc_msg *m = peer_until(dc, "732");
	assert_string_equal(irc_msg_param(m, 1), "a");
	irc_msg_free(m);
	m = peer_until(dc, "732");
	assert_string_equal(irc_msg_param(m, 1), "b");
	irc_msg_free(m);
	m = peer_until(dc, "733");
	irc_msg_free(m);
	fixture_finish(&e);
}

static void test_chat_history_db(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	for (unsigned i = 0; i < 3; i++) {
		AUTOFREE char *line = xasprintf(
			"@time=2023-05-23T06:00:0%u.000Z :foo!u@h PRIVMSG test :msg %u", i, i);
		peer_send(up, line);
	}
	peer_sync(up);
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, true, "");
	static const struct {
		const char *sub, *a, *b;
		unsigned limit, first, count;
	} cases[] = {
		{"BEFORE", "03", nullptr, 100, 0, 3},   {"BEFORE", "00", nullptr, 100, 0, 0},
		{"BEFORE", "01", nullptr, 100, 0, 1},   {"BEFORE", "02", nullptr, 1, 1, 1},
		{"AFTER", "-1", nullptr, 100, 0, 3},    {"AFTER", "02", nullptr, 100, 0, 0},
		{"AFTER", "00", nullptr, 100, 1, 2},    {"AFTER", "00", nullptr, 1, 1, 1},
		{"BETWEEN", "-1", "03", 100, 0, 3},     {"BETWEEN", "03", "-1", 100, 0, 3},
		{"BETWEEN", "00", "02", 100, 1, 1},     {"BETWEEN", "02", "00", 100, 1, 1},
		{"BETWEEN", "00", "03", 100, 1, 2},     {"BETWEEN", "03", "00", 100, 1, 2},
		{"BETWEEN", "-1", "02", 100, 0, 2},     {"BETWEEN", "02", "-1", 100, 0, 2},
		{"BETWEEN", "00", "03", 1, 1, 1},       {"AROUND", "01", nullptr, 100, 0, 3},
		{"AROUND", "01.500", nullptr, 2, 1, 2},
	};
	for (size_t i = 0; i < sizeof(cases) / sizeof(*cases); i++) {
		const char *a =
			cstr_eq(cases[i].a, "-1") ? "timestamp=2023-05-23T05:59:59.000Z" : nullptr;
		AUTOFREE char *bound_a =
			a != nullptr ? xstrdup(a)
				     : xasprintf("timestamp=2023-05-23T06:00:%sZ", cases[i].a);
		const char *b = cases[i].b;
		AUTOFREE char *bound_b = b == nullptr ? nullptr
					 : cstr_eq(b, "-1")
						 ? xstrdup("timestamp=2023-05-23T05:59:59.000Z")
						 : xasprintf("timestamp=2023-05-23T06:00:%sZ", b);
		AUTOFREE char *limit = xasprintf("%u", cases[i].limit);
		struct irc_msg *request = IRC_MSG("CHATHISTORY", cases[i].sub, "foo", bound_a);
		if (bound_b != nullptr) {
			irc_msg_add_param(request, bound_b);
		}
		irc_msg_add_param(request, limit);
		peer_send_take(dc, request);
		irc_msg_vec result = {0};
		peer_roundtrip(dc, &result);
		assert_int_equal(result.len, cases[i].count);
		for (size_t j = 0; j < result.len; j++) {
			AUTOFREE char *text = xasprintf("msg %zu", cases[i].first + j);
			assert_string_equal(irc_msg_param(result.data[j], 1), text);
		}
		irc_msg_vec_free(&result);
	}
	fixture_finish(&e);
}

static void test_statusmsg_keeps_sigil(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "STATUSMSG=@+");
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, true, "");
	peer_send(up, ":bob!u@h PRIVMSG @#chan :ops only");
	struct irc_msg *m = peer_until(dc, "PRIVMSG");
	assert_string_equal(irc_msg_param(m, 0), "@#chan");
	irc_msg_free(m);
	peer_sync(up);
	peer_send(dc, "CHATHISTORY LATEST #chan * 10");
	irc_msg_vec history = {0};
	peer_roundtrip(dc, &history);
	assert_int_equal(history.len, 1);
	assert_string_equal(irc_msg_param(history.data[0], 0), "@#chan");
	irc_msg_vec_free(&history);
	fixture_finish(&e);
}

static void test_upstream_malformed_messages(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	const char *lines[] = {
		":bob JOIN", ":bob PART",          ":bob KICK #x",     ":bob NICK",
		":bob MODE", ":server 353 test =", ":server 332 test", ":bob PRIVMSG #x",
		"CAP",       "AUTHENTICATE",       "BATCH -unknown"};
	for (size_t i = 0; i < sizeof(lines) / sizeof(*lines); i++) {
		peer_send(up, lines[i]);
	}
	peer_sync(up);
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, true, "");
	peer_send(up, ":bob!u@h PRIVMSG test :still alive");
	struct irc_msg *m = peer_until(dc, "PRIVMSG");
	assert_string_equal(irc_msg_param(m, 1), "still alive");
	irc_msg_free(m);
	fixture_finish(&e);
}

static void test_cap_req_removal(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *dc = fixture_connect(&e);
	peer_send(dc, "CAP LS 302");
	for (;;) {
		struct irc_msg *ls = peer_until(dc, "CAP");
		bool last = ls->params.len < 4;
		irc_msg_free(ls);
		if (last) {
			break;
		}
	}
	peer_send(dc, "CAP REQ :batch server-time");
	struct irc_msg *m = peer_until(dc, "CAP");
	assert_string_equal(irc_msg_param(m, 1), "ACK");
	irc_msg_free(m);
	peer_send(dc, "CAP REQ -batch");
	m = peer_until(dc, "CAP");
	assert_string_equal(irc_msg_param(m, 1), "ACK");
	irc_msg_free(m);
	peer_send(dc, "CAP REQ :batch bogus-cap");
	m = peer_until(dc, "CAP");
	assert_string_equal(irc_msg_param(m, 1), "NAK");
	irc_msg_free(m);
	peer_send(dc, "CAP LIST");
	m = peer_until(dc, "CAP");
	assert_string_equal(irc_msg_param(m, 2), "cap-notify server-time");
	irc_msg_free(m);
	fixture_finish(&e);
}

static void test_sasl_errors(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *dc = fixture_connect(&e);
	peer_send(dc, "AUTHENTICATE PLAIN");
	struct irc_msg *m = peer_until(dc, "904");
	assert_string_equal(irc_msg_param(m, 1),
			    "AUTHENTICATE requires the \"sasl\" capability to be enabled");
	irc_msg_free(m);
	peer_send(dc, "CAP REQ sasl");
	m = peer_until(dc, "CAP");
	irc_msg_free(m);
	peer_send(dc, "AUTHENTICATE PLAIN");
	m = peer_until(dc, "AUTHENTICATE");
	irc_msg_free(m);
	peer_send(dc, "AUTHENTICATE !!!!");
	m = peer_until(dc, "904");
	assert_string_equal(irc_msg_param(m, 1), "Invalid base64-encoded response");
	irc_msg_free(m);
	peer_send(dc, "AUTHENTICATE PLAIN");
	m = peer_until(dc, "AUTHENTICATE");
	irc_msg_free(m);
	peer_send(dc, "AUTHENTICATE AHRlc3QAc2VjcmV0");
	m = peer_until(dc, "903");
	irc_msg_free(m);
	fixture_finish(&e);
}

static void test_upstream_clock_skew(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	struct timestamp ahead = timestamp_now();
	ahead.sec += 3600;
	AUTOFREE char *time = format_server_time_cstr(ahead);
	struct irc_msg *text = IRC_MSG("PRIVMSG", "test", "from the future");
	irc_msg_set_prefix(text, "foo", "u", "h");
	irc_tags_set(&text->tags, "time", time);
	peer_send_take(up, text);
	peer_sync(up);
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, true, "draft/read-marker");
	peer_send(dc, "CHATHISTORY LATEST foo * 10");
	irc_msg_vec history = {0};
	peer_roundtrip(dc, &history);
	assert_int_equal(history.len, 1);
	assert_string_equal(irc_msg_param(history.data[0], 1), "from the future");
	irc_msg_vec_free(&history);
	AUTOFREE char *bound = xasprintf("timestamp=%s", time);
	peer_send_take(dc, IRC_MSG("MARKREAD", "foo", bound));
	struct irc_msg *m = peer_until(dc, "MARKREAD");
	assert_string_equal(irc_msg_param(m, 1), bound);
	irc_msg_free(m);
	ahead.sec += 3600;
	AUTOFREE char *later = format_server_time_cstr(ahead);
	AUTOFREE char *later_bound = xasprintf("timestamp=%s", later);
	peer_send_take(dc, IRC_MSG("MARKREAD", "foo", later_bound));
	m = peer_until(dc, "MARKREAD");
	assert_string_equal(irc_msg_param(m, 1), bound);
	irc_msg_free(m);
	peer_send_take(dc, IRC_MSG("MARKREAD", "#empty", later_bound));
	m = peer_until(dc, "MARKREAD");
	struct timestamp clamped;
	assert_true(parse_chathistory_bound(str_from(irc_msg_param(m, 1)), &clamped));
	assert_true(timestamp_cmp(clamped, timestamp_now()) <= 0);
	irc_msg_free(m);
	fixture_finish(&e);
}

static void test_chat_history_limit(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct server_config *cfg = server_config_default();
	cfg->cfg.basic.msg_store_driver = MSG_STORE_DB;
	cfg->cfg.basic.chat_history_limit = 2;
	server_config_set(&e.srv, cfg);
	struct test_peer *up = fixture_upstream(&e, "", "");
	for (unsigned i = 0; i < 4; i++) {
		AUTOFREE char *line = xasprintf(
			"@time=2023-01-01T00:00:0%u.000Z :foo PRIVMSG test :msg %u", i, i);
		peer_send(up, line);
	}
	peer_sync(up);
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, true, "batch server-time draft/chathistory");
	peer_send(dc, "CHATHISTORY LATEST foo * 1000");
	irc_msg_vec history = {0};
	peer_roundtrip(dc, &history);
	assert_int_equal(history.len, 2);
	assert_string_equal(irc_msg_param(history.data[0], 1), "msg 2");
	assert_string_equal(irc_msg_param(history.data[1], 1), "msg 3");
	irc_msg_vec_free(&history);
	fixture_finish(&e);
}

static struct irc_msg *until_tagged(struct test_peer *p, const char *cmd, const char *tag) {
	for (;;) {
		struct irc_msg *m = peer_until(p, cmd);
		const char *value = irc_tags_get(&m->tags, "soju.im/network");
		if (value != nullptr && cstr_eq(value, tag)) {
			return m;
		}
		irc_msg_free(m);
	}
}
static void test_multiplex(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, false,
			 "batch cap-notify message-tags soju.im/bouncer-networks "
			 "soju.im/bouncer-networks-notify soju.im/multiplex");
	AUTOFREE char *id = xasprintf("%lld", (long long)e.network.id);
	peer_send_take(dc, IRC_MSG("BOUNCER", "ATTACH", id));
	struct irc_msg *m = until_tagged(dc, "001", id);
	assert_string_equal(irc_msg_param(m, 0), "test");
	irc_msg_free(m);
	m = until_tagged(dc, "422", id);
	irc_msg_free(m);
	struct irc_msg *request = IRC_MSG("PRIVMSG", "#chan", "hello");
	irc_tags_set(&request->tags, "soju.im/network", id);
	peer_send_take(dc, request);
	m = peer_until(up, "PRIVMSG");
	assert_string_equal(irc_msg_param(m, 1), "hello");
	assert_null(irc_tags_get(&m->tags, "soju.im/network"));
	irc_msg_free(m);
	peer_send(up, ":alice!a@h PRIVMSG test :hi there");
	m = until_tagged(dc, "PRIVMSG", id);
	assert_string_equal(irc_msg_param(m, 1), "hi there");
	irc_msg_free(m);
	peer_send(dc, "@soju.im/network=12345 PRIVMSG #chan x");
	m = peer_until(dc, "FAIL");
	assert_string_equal(irc_msg_param(m, 1), "NETWORK_NOT_ATTACHED");
	irc_msg_free(m);
	peer_send_take(dc, IRC_MSG("BOUNCER", "DETACH", id));
	peer_sync(dc);
	peer_send(up, ":alice!a@h PRIVMSG test :missed");
	peer_sync(up);
	irc_msg_vec replies = {0};
	peer_roundtrip(dc, &replies);
	for (size_t i = 0; i < replies.len; i++) {
		assert_null(irc_tags_get(&replies.data[i]->tags, "soju.im/network"));
	}
	irc_msg_vec_free(&replies);
	peer_send_take(dc, IRC_MSG("BOUNCER", "ATTACH", id));
	m = until_tagged(dc, "422", id);
	irc_msg_free(m);
	peer_send_take(dc, IRC_MSG("BOUNCER", "DELNETWORK", id));
	m = until_tagged(dc, "ERROR", id);
	irc_msg_free(m);
	peer_sync(dc);
	fixture_finish(&e);
}

static void test_multiplex_unsupported(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	(void)up;
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, false, "soju.im/bouncer-networks");
	AUTOFREE char *id = xasprintf("%lld", (long long)e.network.id);
	peer_send_take(dc, IRC_MSG("BOUNCER", "ATTACH", id));
	struct irc_msg *m = peer_until(dc, "FAIL");
	assert_string_equal(irc_msg_param(m, 1), "UNKNOWN_COMMAND");
	irc_msg_free(m);
	dc = fixture_connect(&e);
	fixture_register(&e, dc, true, "soju.im/multiplex");
	peer_send_take(dc, IRC_MSG("BOUNCER", "ATTACH", id));
	m = peer_until(dc, "FAIL");
	assert_string_equal(irc_msg_param(m, 1), "INVALID_CONNECTION");
	irc_msg_free(m);
	fixture_finish(&e);
}

static void test_multiline(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up =
		fixture_upstream(&e,
				 "batch echo-message labeled-response message-tags server-time "
				 "draft/multiline=max-bytes=4096,max-lines=4",
				 "");
	struct test_peer *ml = fixture_connect(&e), *plain = fixture_connect(&e);
	fixture_register(&e, ml, true,
			 "batch echo-message labeled-response message-tags draft/chathistory "
			 "draft/multiline");
	fixture_register(&e, plain, true, "server-time");
	const char *lines[] = {"@msgid=m1 :bob!b@h BATCH +x draft/multiline #c",
			       "@batch=x :bob!b@h PRIVMSG #c hello",
			       "@batch=x;draft/multiline-concat :bob!b@h PRIVMSG #c : world",
			       "@batch=x :bob!b@h PRIVMSG #c :",
			       "@batch=x :bob!b@h PRIVMSG #c bye",
			       "BATCH -x"};
	for (size_t i = 0; i < sizeof(lines) / sizeof(*lines); i++) {
		peer_send(up, lines[i]);
	}
	struct irc_msg *m = peer_until(ml, "BATCH");
	assert_string_equal(irc_msg_param(m, 1), "draft/multiline");
	assert_string_equal(irc_tags_get(&m->tags, "msgid"), "m1");
	irc_msg_free(m);
	const char *texts[] = {"hello world", "", "bye"};
	for (size_t i = 0; i < 3; i++) {
		m = peer_until(ml, "PRIVMSG");
		assert_string_equal(irc_msg_param(m, 1), texts[i]);
		irc_msg_free(m);
	}
	m = peer_until(plain, "PRIVMSG");
	assert_string_equal(irc_msg_param(m, 1), "hello world");
	irc_msg_free(m);
	m = peer_until(plain, "PRIVMSG");
	assert_string_equal(irc_msg_param(m, 1), "bye");
	irc_msg_free(m);
	peer_send(ml, "@label=l1;+draft/reply=m1 BATCH +a draft/multiline #c");
	peer_send(ml, "@batch=a PRIVMSG #c one");
	peer_send(ml, "@batch=a PRIVMSG #c two");
	peer_send(ml, "BATCH -a");
	m = peer_until(up, "BATCH");
	assert_string_equal(irc_msg_param(m, 1), "draft/multiline");
	AUTOFREE char *label = xstrdup(irc_tags_get(&m->tags, "label"));
	assert_string_equal(irc_tags_get(&m->tags, "+draft/reply"), "m1");
	irc_msg_free(m);
	peer_sync(up);
	m = IRC_MSG("BATCH", "+e", "draft/multiline", "#c");
	irc_msg_set_prefix(m, "test", "u", "h");
	irc_tags_set(&m->tags, "label", label);
	irc_tags_set(&m->tags, "msgid", "m2");
	peer_send_take(up, m);
	peer_send(up, "@batch=e :test!u@h PRIVMSG #c one");
	peer_send(up, "@batch=e :test!u@h PRIVMSG #c two");
	peer_send(up, "BATCH -e");
	m = peer_until(ml, "BATCH");
	if (irc_msg_param(m, 0)[0] == '-') {
		irc_msg_free(m);
		m = peer_until(ml, "BATCH");
	}
	assert_string_equal(irc_tags_get(&m->tags, "label"), "l1");
	irc_msg_free(m);
	peer_sync(ml);
	peer_send(ml, "@label=l2 BATCH +b draft/multiline #c");
	for (unsigned i = 0; i < 5; i++) {
		peer_send(ml, "@batch=b PRIVMSG #c x");
	}
	peer_send(ml, "BATCH -b");
	m = peer_until(ml, "FAIL");
	assert_string_equal(irc_tags_get(&m->tags, "label"), "l2");
	assert_string_equal(irc_msg_param(m, 1), "MULTILINE_MAX_LINES");
	assert_string_equal(irc_msg_param(m, 2), "4");
	irc_msg_free(m);
	fixture_finish(&e);
}

struct created_users {
	atomic_uint done;
	atomic_bool failed;
	int64_t ids[8];
};
struct created_call {
	struct created_users *results;
	size_t index;
};
static void created_user(void *data, int64_t id, const struct err *err) {
	struct created_call *call = data;
	call->results->ids[call->index] = id;
	if (err != nullptr) {
		atomic_store(&call->results->failed, true);
	}
	atomic_fetch_add(&call->results->done, 1);
}
struct user_count {
	size_t count;
};
static void count_users(struct db *db, void *data) {
	struct user_count *count = data;
	db_user_vec users = {0};
	bool ok = db_list_users(db, &users, nullptr);
	count->count = ok ? users.len : 0;
	db_user_vec_free(&users);
}
static void test_create_user_on_auth_concurrently(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct server_config *cfg = server_config_default();
	cfg->cfg.basic.enable_users_on_auth = true;
	server_config_set(&e.srv, cfg);
	struct created_users results = {0};
	struct created_call calls[8];
	for (size_t i = 0; i < 8; i++) {
		calls[i] = (struct created_call){.results = &results, .index = i};
		server_get_or_create_user(&e.srv, "alice", created_user, &calls[i]);
	}
	uint64_t deadline = uv_hrtime() + 10000000000ULL;
	while (atomic_load(&results.done) < 8 && uv_hrtime() < deadline) {
		usleep(1000);
	}
	assert_int_equal(atomic_load(&results.done), 8);
	assert_false(atomic_load(&results.failed));
	for (size_t i = 0; i < 8; i++) {
		assert_int_equal(results.ids[i], results.ids[0]);
	}
	struct user_count count = {0};
	db_thread_call(e.db, count_users, &count);
	assert_int_equal(count.count, 2); // fixture user and the one new account
	fixture_finish(&e);
}
static void check_store(struct user *u, void *data) {
	bool *memory = data;
	*memory = u->store.db == nullptr;
}
static void test_msg_store_not_reloaded(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	(void)up;
	struct server_config *cfg = server_config_default();
	cfg->cfg.basic.msg_store_driver = MSG_STORE_MEMORY;
	xfree(cfg->cfg.basic.title);
	cfg->cfg.basic.title = xstrdup("reloaded");
	server_config_reload(&e.srv, cfg);
	cfg = server_config_get(&e.srv);
	assert_int_equal(cfg->cfg.basic.msg_store_driver, MSG_STORE_DB);
	assert_string_equal(cfg->cfg.basic.title, "reloaded");
	server_config_unref(cfg);
	bool memory = true;
	fixture_run_user(&e, check_store, &memory);
	assert_false(memory);
	fixture_finish(&e);
}
static void test_pending_job_disconnects(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	(void)up;
	for (unsigned i = 0; i < 16; i++) {
		struct test_peer *dc = fixture_connect(&e);
		peer_send(dc, "PASS secret");
		peer_send(dc, "NICK test");
		peer_send(dc, "USER test/testnet 0 * :Test");
		peer_send(dc, "CHATHISTORY LATEST #x * 1000");
		close(dc->fd);
		dc->fd = -1;
	}
	fixture_finish(&e);
}

static void client_count(struct user *u, void *data) {
	*(size_t *)data = u->clients.len;
}
static void test_memory_backlog(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, true);
	struct test_peer *up = fixture_upstream(&e, "", "");
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, true, "");
	peer_send(up, ":foo!u@h PRIVMSG test :online");
	struct irc_msg *m = peer_until(dc, "PRIVMSG");
	irc_msg_free(m);
	peer_sync(dc);
	close(dc->fd);
	dc->fd = -1;
	size_t count = 1;
	uint64_t deadline = uv_hrtime() + 5000000000ULL;
	while (count != 0 && uv_hrtime() < deadline) {
		fixture_run_user(&e, client_count, &count);
	}
	assert_int_equal(count, 0);
	peer_send(up, ":foo!u@h PRIVMSG test :offline");
	peer_sync(up);
	dc = fixture_connect(&e);
	peer_send(dc, "PASS secret");
	peer_send(dc, "NICK test");
	peer_send(dc, "USER test/testnet 0 * :Test");
	m = peer_until(dc, "001");
	irc_msg_free(m);
	m = peer_until(dc, "PRIVMSG");
	assert_string_equal(irc_msg_param(m, 1), "offline");
	irc_msg_free(m);
	fixture_finish(&e);
}

static char *service_reply(struct test_peer *dc) {
	for (;;) {
		struct irc_msg *m = peer_until(dc, "PRIVMSG");
		if (m->prefix != nullptr && cstr_eq(m->prefix->name, "BouncerServ")) {
			char *text = xstrdup(irc_msg_param(m, 1));
			irc_msg_free(m);
			return text;
		}
		irc_msg_free(m);
	}
}
static void service_expect(struct test_peer *dc, const char *command, const char *expected) {
	peer_send_take(dc, IRC_MSG("PRIVMSG", "BouncerServ", command));
	AUTOFREE char *reply = service_reply(dc);
	assert_string_equal(reply, expected);
}
struct store_record {
	struct db_user record;
	struct err *err;
};
static void store_record_db(struct db *db, void *data) {
	struct store_record *j = data;
	bool ok = db_store_user(db, &j->record, &j->err);
	(void)ok;
}
static void make_admin(struct user *u, void *data) {
	(void)data;
	u->record.admin = true;
	struct store_record j = {0};
	db_user_copy(&j.record, &u->record);
	db_thread_call(u->srv->db, store_record_db, &j);
	assert_null(j.err);
	db_user_free(&j.record);
}
static void add_other(struct user *u, void *data) {
	(void)data;
	struct store_record j = {0};
	db_user_init(&j.record, "other");
	db_thread_call(u->srv->db, store_record_db, &j);
	assert_null(j.err);
	struct db_user *record = xmalloc(sizeof(*record));
	db_user_copy(record, &j.record);
	user_start_take(u->srv, record);
	db_user_free(&j.record);
}
static void test_service_other_user(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	(void)up;
	fixture_run_user(&e, make_admin, nullptr);
	fixture_run_user(&e, add_other, nullptr);
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, true, "");
	service_expect(dc, "user update other -max-networks 5", "updated user \"other\"");
	service_expect(dc, "user run other user update -max-networks 3", "updated user \"other\"");
	service_expect(dc, "user run other network status",
		       "No network configured, add one with \"network create\".");
	service_expect(dc, "user delete other d0941e", "deleted user \"other\"");
	int64_t id = 0;
	assert_false(server_find_user(&e.srv, "other", &id));
	fixture_finish(&e);
}
struct check_password {
	const char *username, *password;
	bool found, ok;
	struct err *err;
};
static void check_password_db(struct db *db, void *data) {
	struct check_password *j = data;
	struct db_user u;
	bool upgraded = false;
	j->ok = db_get_user(db, j->username, &u, &j->found, &j->err);
	if (j->found) {
		j->ok = j->ok && db_user_check_password(&u, j->password, &upgraded, &j->err);
		db_user_free(&u);
	}
}
static void check_cert(struct user *u, void *data) {
	(void)data;
	struct user_network *net = u->networks.data[0];
	assert_string_equal(net->record.sasl.mechanism, "EXTERNAL");
	assert_true(net->record.sasl.external_key.set);
	assert_true(net->record.sasl.external_cert.set);
}
static void test_service_blocking_work(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	(void)up;
	fixture_run_user(&e, make_admin, nullptr);
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, true, "");
	service_expect(dc, "certfp generate -key-type ed25519", "certificate generated");
	const char *const labels[] = {
		"SHA-1 fingerprint: ", "SHA-256 fingerprint: ", "SHA-512 fingerprint: "};
	for (size_t i = 0; i < 3; i++) {
		AUTOFREE char *line = service_reply(dc);
		assert_true(str_starts_with(str_from(line), str_from(labels[i])));
	}
	fixture_run_user(&e, check_cert, nullptr);
	service_expect(dc, "user update -password newpass", "updated user \"test\"");
	struct check_password j = {.username = "test", .password = "newpass"};
	db_thread_call(e.db, check_password_db, &j);
	assert_true(j.ok);
	assert_null(j.err);
	service_expect(dc, "user create -username created -password pw",
		       "created user \"created\"");
	int64_t id = 0;
	assert_true(server_find_user(&e.srv, "created", &id));
	j = (struct check_password){.username = "created", .password = "pw"};
	db_thread_call(e.db, check_password_db, &j);
	assert_true(j.ok);
	assert_null(j.err);
	fixture_finish(&e);
}
static void test_service_commands(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, true);
	struct test_peer *up = fixture_upstream(&e, "", "");
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, true, "");
	service_expect(dc, "s",
		       "error: command \"s\" is ambiguous (type \"help\" for a list of commands)");
	service_expect(dc, "server status", "error: you must be an admin to use this command");
	service_expect(dc, "network create -addr irc+insecure://example -ignore-limit",
		       "error: you must be an admin to use the flag -ignore-limit");
	service_expect(dc, "user update -admin true", "error: cannot update -admin of own user");
	service_expect(dc, "user update -max-networks 2",
		       "error: cannot update -max-networks of own user");
	service_expect(dc, "user update somebody -password x",
		       "error: you must be an admin to update other users");
	service_expect(dc, "user delete somebody", "error: only admins may delete other users");
	service_expect(dc,
		       "channel create #saved -detached true -detach-after 1h30m -relay-detached "
		       "highlight",
		       "created channel \"#saved\"");
	struct irc_msg *m = peer_until(up, "JOIN");
	assert_string_equal(irc_msg_param(m, 0), "#saved");
	irc_msg_free(m);
	service_expect(dc, "channel status",
		       "#saved [parted, detached]: relay-detached: highlight, reattach-on: "
		       "default, detach-after: 1h30m0s, detach-on: default");
	service_expect(
		dc, "channel update #saved -reattach-on message -detach-on none -detach-after 1.5s",
		"updated channel \"#saved\"");
	service_expect(dc, "channel update #saved -detach-after 9223372036854775807h",
		       "error: unknown duration for -detach-after \"9223372036854775807h\" "
		       "(duration format: 0, 300s, 22h30m, ...)");
	service_expect(dc, "channel delete #saved", "deleted channel \"#saved\"");
	service_expect(dc, "sasl set-plain account secret", "credentials saved");
	service_expect(dc, "sasl status", "SASL PLAIN enabled with username \"account\"");
	AUTOFREE char *line = service_reply(dc);
	assert_string_equal(line, "Unauthenticated on upstream network");
	service_expect(dc, "sasl reset", "credentials reset");
	service_expect(dc,
		       "net crea -addr irc+insecure://example -name example -enabled false "
		       "-connect-command 'PRIVMSG NickServ :IDENTIFY password'",
		       "created network \"example\"");
	service_expect(dc, "network update example -name renamed -certfp 1234",
		       "error: the certificate fingerprint must be a SHA256 or SHA512 hash");
	service_expect(dc, "network update example -name renamed", "updated network \"renamed\"");
	peer_send(dc, "PRIVMSG BouncerServ :network info renamed");
	bool redacted = false;
	for (size_t i = 0; i < 9; i++) {
		AUTOFREE char *reply = service_reply(dc);
		assert_null(strstr(reply, "password"));
		if (cstr_eq(reply, "connect-command: PRIVMSG NickServ (parameters hidden)")) {
			redacted = true;
		}
	}
	assert_true(redacted);
	service_expect(dc, "network delete renamed", "deleted network \"renamed\"");
	service_expect(dc, "net quote 'WHO test'", "sent command to \"testnet\"");
	m = peer_until(up, "WHO");
	assert_string_equal(irc_msg_param(m, 0), "test");
	irc_msg_free(m);
	service_expect(dc, "device-certificate create -label phone",
		       "error: client certification authentication is disabled");
	fixture_finish(&e);
}
static void enable_cert_auth(struct user *u, void *data) {
	(void)data;
	struct server_config *cfg = server_config_default();
	cfg->cfg.basic.client_cert_auth = true;
	server_config_reload(u->srv, cfg);
}
static void test_service_device_certificates(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	(void)up;
	fixture_run_user(&e, enable_cert_auth, nullptr);
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, false, "");
	const char *fp = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
			 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
	AUTOFREE char *command =
		xasprintf("device-certificate create -fingerprint %s -label phone", fp);
	AUTOFREE char *reply = xasprintf("created device certificate \"%s\"", fp);
	service_expect(dc, command, reply);
	service_expect(dc, command, "error: this device certificate fingerprint already exists");
	service_expect(dc, "device-certificate delete aaaaaaaaaaaa",
		       "deleted device certificate \"aaaaaaaaaaaa\"");
	service_expect(dc, "device-certificate status", "No device certificate configured.");
	fixture_finish(&e);
}
static void test_service_admin_races(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	(void)up;
	fixture_run_user(&e, make_admin, nullptr);
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, false, "");
	service_expect(dc, "user create -username other -disable-password",
		       "created user \"other\"");
	// The expensive command completes after its target has been removed.
	peer_send(dc, "PRIVMSG BouncerServ :user update other -password latepass");
	peer_send(dc, "PRIVMSG BouncerServ :user delete other d0941e");
	bool deleted = false, stopped = false;
	for (size_t i = 0; i < 2; i++) {
		AUTOFREE char *line = service_reply(dc);
		if (cstr_eq(line, "deleted user \"other\"")) {
			deleted = true;
		} else if (cstr_eq(line, "error: user stopped")) {
			stopped = true;
		} else {
			assert_string_equal(line, "updated user \"other\"");
		}
	}
	assert_true(deleted);
	(void)stopped;
	// Shutdown must drain deferred key/password work without retaining clients.
	peer_send(dc, "PRIVMSG BouncerServ :user update -password shutdownpass");
	peer_send(dc, "PRIVMSG BouncerServ :certfp generate -network testnet -key-type ed25519");
	peer_sync(dc);
	close(dc->fd);
	dc->fd = -1;
	fixture_finish(&e);
}

struct global_result {
	atomic_bool done;
	struct sloop *origin;
	strvec lines;
	const char *command;
};
static void global_print(void *data, const char *line) {
	struct global_result *r = data;
	assert_ptr_equal(loop_current(), r->origin);
	strvec_push(&r->lines, line);
}
static void global_done(void *data) {
	struct global_result *r = data;
	assert_ptr_equal(loop_current(), r->origin);
	atomic_store(&r->done, true);
}
static void global_start(struct user *u, void *data) {
	struct global_result *r = data;
	r->origin = loop_current();
	service_global(u->srv, true, r->command, global_print, global_done, r);
}
static void wait_global(struct server_fixture *e, struct global_result *r, const char *command) {
	r->command = command;
	atomic_store(&r->done, false);
	fixture_run_user(e, global_start, r);
	uint64_t until = uv_hrtime() + 10000000000ULL;
	while (!atomic_load(&r->done) && uv_hrtime() < until) {
		usleep(1000);
	}
	assert_true(atomic_load(&r->done));
}
static void test_service_global(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	(void)up;
	struct global_result r = {0};
	wait_global(&e, &r, "user create -username other -disable-password");
	assert_int_equal(r.lines.len, 1);
	assert_string_equal(r.lines.data[0], "created user \"other\"");
	strvec_free(&r.lines);
	wait_global(&e, &r, "user run other user update -max-networks 3");
	assert_int_equal(r.lines.len, 1);
	assert_string_equal(r.lines.data[0], "updated user \"other\"");
	strvec_free(&r.lines);
	wait_global(&e, &r, "network status");
	assert_int_equal(r.lines.len, 1);
	assert_non_null(strstr(r.lines.data[0], "this command must be run as a user"));
	strvec_free(&r.lines);
	wait_global(&e, &r, "server debug true");
	assert_int_equal(r.lines.len, 0);
	assert_true(log_debug_enabled(&e.srv.log));
	wait_global(&e, &r, "server debug false");
	assert_false(log_debug_enabled(&e.srv.log));
	wait_global(&e, &r, "server status");
	assert_int_equal(r.lines.len, 1);
	assert_non_null(strstr(r.lines.data[0], "2/2 users"));
	strvec_free(&r.lines);
	wait_global(&e, &r, "user delete other d0941e");
	assert_int_equal(r.lines.len, 1);
	assert_string_equal(r.lines.data[0], "deleted user \"other\"");
	strvec_free(&r.lines);
	fixture_finish(&e);
}

static void stale_start(struct user *u, void *data) {
	struct db_user *record = data;
	struct db_user *copy = xmalloc(sizeof(*copy));
	db_user_copy(copy, record);
	user_start_take(u->srv, copy);
}
static void test_service_deleted_identity(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	(void)up;
	fixture_run_user(&e, make_admin, nullptr);
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, false, "");
	service_expect(dc, "user create -username other -disable-password",
		       "created user \"other\"");
	int64_t old = 0;
	assert_true(server_find_user(&e.srv, "other", &old));
	struct db_user stale;
	db_user_init(&stale, "other");
	stale.id = old;
	service_expect(dc, "user delete other d0941e", "deleted user \"other\"");
	fixture_run_user(&e, stale_start, &stale);
	peer_sync(dc);
	int64_t id = 0;
	assert_false(server_find_user(&e.srv, "other", &id));
	service_expect(dc, "user create -username other -disable-password",
		       "created user \"other\"");
	assert_true(server_find_user(&e.srv, "other", &id));
	assert_true(id > old);
	fixture_run_user(&e, stale_start, &stale);
	peer_sync(dc);
	assert_true(server_find_user(&e.srv, "other", &id));
	assert_true(id > old);
	db_user_free(&stale);
	fixture_finish(&e);
}
static void test_service_cert_network_race(void **state) {
	(void)state;
	struct server_fixture e;
	fixture_init(&e, false);
	struct test_peer *up = fixture_upstream(&e, "", "");
	(void)up;
	struct test_peer *dc = fixture_connect(&e);
	fixture_register(&e, dc, false, "");
	service_expect(dc, "network create -addr example -name temporary -enabled false",
		       "created network \"temporary\"");
	peer_send(dc, "PRIVMSG BouncerServ :certfp generate -network temporary -key-type ed25519");
	peer_send(dc, "PRIVMSG BouncerServ :network delete temporary");
	bool deleted = false, generated = false, missing = false;
	while (!deleted || (!generated && !missing)) {
		AUTOFREE char *line = service_reply(dc);
		if (cstr_eq(line, "deleted network \"temporary\"")) {
			deleted = true;
		} else if (cstr_eq(line, "certificate generated")) {
			generated = true;
			for (size_t i = 0; i < 3; i++) {
				AUTOFREE char *fp = service_reply(dc);
				assert_non_null(strstr(fp, "fingerprint: "));
			}
		} else {
			assert_string_equal(line, "error: network was deleted");
			missing = true;
		}
	}
	service_expect(dc, "network create -addr example -name temporary -enabled false",
		       "created network \"temporary\"");
	service_expect(dc, "certfp fingerprint -network temporary", "error: CertFP not set up");
	fixture_finish(&e);
}

T_MAIN(cmocka_unit_test(test_service_deleted_identity),
       cmocka_unit_test(test_service_cert_network_race), cmocka_unit_test(test_service_global),
       cmocka_unit_test(test_service_other_user), cmocka_unit_test(test_service_blocking_work),
       cmocka_unit_test(test_service_commands), cmocka_unit_test(test_service_device_certificates),
       cmocka_unit_test(test_service_admin_races), cmocka_unit_test(test_memory_backlog),
       cmocka_unit_test(test_create_user_on_auth_concurrently),
       cmocka_unit_test(test_msg_store_not_reloaded),
       cmocka_unit_test(test_pending_job_disconnects), cmocka_unit_test(test_broadcast),
       cmocka_unit_test(test_chat_history_db), cmocka_unit_test(test_chat_history_params),
       cmocka_unit_test(test_read_marker), cmocka_unit_test(test_labeled_response_ack),
       cmocka_unit_test(test_monitor_invalid_targets), cmocka_unit_test(test_statusmsg_keeps_sigil),
       cmocka_unit_test(test_upstream_malformed_messages), cmocka_unit_test(test_cap_req_removal),
       cmocka_unit_test(test_sasl_errors), cmocka_unit_test(test_upstream_clock_skew),
       cmocka_unit_test(test_chat_history_limit), cmocka_unit_test(test_multiplex),
       cmocka_unit_test(test_multiplex_unsupported), cmocka_unit_test(test_multiline))
