#include "testsupport.h"

#include <arpa/inet.h>
#include <fcntl.h>
#include <sys/socket.h>
#include <unistd.h>

#include "base/alloc.h"
#include "upstream/upstream.h"

typedef VEC(uint64_t) id_vec;
struct env {
	struct loop_pool loops;
	struct server srv;
	struct upstream *uc;
	irc_msg_vec sent, received;
	id_vec ids;
	bool closed;
	size_t detached;
};

static void sent(struct upstream *uc, struct irc_msg *msg, void *data) {
	(void)uc;
	struct env *e = data;
	vec_push(&e->sent, msg);
}

static void received(struct upstream *uc, struct irc_msg *msg, uint64_t dc_id, void *data) {
	(void)uc;
	struct env *e = data;
	vec_push(&e->received, msg);
	vec_push(&e->ids, dc_id);
}

static void closed(struct upstream *uc, const char *reason, bool temporary, void *data) {
	(void)uc;
	(void)reason;
	(void)temporary;
	struct env *e = data;
	e->closed = true;
}

static void detach(struct upstream *uc, const char *channel, void *data) {
	(void)uc;
	assert_string_equal(channel, "#chan");
	struct env *e = data;
	e->detached++;
}

static void setup(struct env *e) {
	*e = (struct env){0};
	loop_pool_init(&e->loops, 1);
	loop_set_current(&e->loops.loops[0]);
	server_init(&e->srv, &e->loops, nullptr);
	struct db_user u;
	struct db_network n;
	db_user_init(&u, "me");
	db_network_init(&n, "irc+insecure://127.0.0.1:1");
	e->uc = upstream_new(&e->srv, &u, &n,
			     &(struct upstream_events){.send = sent,
						       .message = received,
						       .closed = closed,
						       .detach = detach,
						       .data = e});
	db_user_free(&u);
	db_network_free(&n);
}

static void teardown(struct env *e) {
	upstream_free(e->uc);
	server_finish(&e->srv);
	loop_close_inbox(&e->loops.loops[0]);
	uv_run(&e->loops.loops[0].uv, UV_RUN_DEFAULT);
	loop_set_current(nullptr);
	loop_pool_free(&e->loops);
	irc_msg_vec_free(&e->sent);
	irc_msg_vec_free(&e->received);
	vec_free(&e->ids);
}

static void feed(struct env *e, const char *line) {
	struct irc_msg *msg = irc_msg_parse(str_from(line), nullptr);
	assert_non_null(msg);
	upstream_receive_take(e->uc, msg);
}

static void online(struct env *e) {
	feed(e, ":server 001 me :Welcome");
}

static void test_errno_messages(void **state) {
	(void)state;
	AUTOFREE char *a = go_errno_message(UV_ECONNREFUSED);
	AUTOFREE char *b = go_errno_message(UV_ENOENT);
	assert_string_equal(a, "connection refused");
	assert_string_equal(b, "no such file or directory");
}

static void test_labels(void **state) {
	(void)state;
	uint64_t id;
	char *label;
	assert_true(upstream_parse_label("", &id, &label, nullptr));
	assert_int_equal(id, 0);
	assert_string_equal(label, "");
	xfree(label);
	assert_true(upstream_parse_label("sd-12-3-abc-d", &id, &label, nullptr));
	assert_int_equal(id, 12);
	assert_string_equal(label, "abc-d");
	xfree(label);
	assert_false(upstream_parse_label("sd-x-1-a", &id, &label, nullptr));
	assert_false(upstream_parse_label("xx-1-1-a", &id, &label, nullptr));
	assert_false(upstream_parse_label("sd-1", &id, &label, nullptr));
}

static void test_registration_errors(void **state) {
	(void)state;
	struct irc_msg *m = IRC_MSG("464", "*", "Password incorrect");
	assert_false(upstream_registration_temporary(m));
	AUTOFREE char *s = upstream_registration_error(m);
	assert_string_equal(s, "registration error (464): Password incorrect");
	irc_msg_free(m);
	m = IRC_MSG("FAIL", "*", "ACCOUNT_REQUIRED", "Login first");
	assert_false(upstream_registration_temporary(m));
	irc_msg_free(m);
	m = IRC_MSG("433", "*", "x", "in use");
	assert_true(upstream_registration_temporary(m));
	irc_msg_free(m);
}

static void test_caps_requested(void **state) {
	(void)state;
	struct cap_registry caps;
	cap_registry_init(&caps);
	cap_registry_set_available(&caps, "batch", "");
	cap_registry_set_available(&caps, "echo-message", "");
	cap_registry_set_available(&caps, "labeled-response", "");
	cap_registry_set_available(&caps, "foo", "");
	strvec requested = {0};
	upstream_caps_to_request(&caps, &requested);
	assert_int_equal(requested.len, 3);
	assert_string_equal(requested.data[0], "batch");
	assert_string_equal(requested.data[1], "labeled-response");
	assert_string_equal(requested.data[2], "echo-message");
	strvec_free(&requested);
	cap_registry_del(&caps, "labeled-response");
	upstream_caps_to_request(&caps, &requested);
	assert_int_equal(requested.len, 1);
	assert_string_equal(requested.data[0], "batch");
	strvec_free(&requested);
	cap_registry_set_enabled(&caps, "echo-message", true);
	upstream_caps_to_request(&caps, &requested);
	assert_int_equal(requested.len, 2);
	assert_string_equal(requested.data[1], "-echo-message");
	strvec_free(&requested);
	cap_registry_free(&caps);
}

static void test_cap_ls_continuation(void **state) {
	(void)state;
	struct env e;
	setup(&e);
	upstream_register(e.uc);
	assert_int_equal(e.sent.len, 3);
	feed(&e, "CAP * LS * :batch message-tags");
	assert_int_equal(e.sent.len, 3);
	feed(&e, "CAP * LS :labeled-response echo-message");
	assert_int_equal(e.sent.len, 5);
	assert_msg_str(e.sent.data[3], "CAP REQ :batch labeled-response message-tags echo-message");
	assert_msg_str(e.sent.data[4], "CAP END");
	teardown(&e);
}

static void test_whox_fields(void **state) {
	(void)state;
	struct upstream_user u = {.nickname = (char *)(uintptr_t)"n",
				  .username = (char *)(uintptr_t)"u",
				  .realname = (char *)(uintptr_t)""};
	assert_true(upstream_user_has_whox_fields(&u, "tnu"));
	assert_false(upstream_user_has_whox_fields(&u, "nr"));
}

static void test_registration_timeout(void **state) {
	(void)state;
	struct env e;
	setup(&e);
	upstream_register(e.uc);
	upstream_tick(e.uc, e.uc->deadline - 1);
	assert_false(e.closed);
	upstream_tick(e.uc, e.uc->deadline);
	assert_true(e.closed);
	teardown(&e);
}

static void test_sasl_cap_nak_ends_capability_negotiation(void **state) {
	(void)state;
	struct env e;
	setup(&e);
	xfree(e.uc->network.sasl.mechanism);
	e.uc->network.sasl.mechanism = xstrdup("PLAIN");
	upstream_register(e.uc);
	feed(&e, "CAP * LS :sasl");
	assert_msg_str(vec_last(&e.sent), "CAP REQ sasl");
	feed(&e, "CAP * NAK sasl");
	assert_msg_str(vec_last(&e.sent), "CAP END");
	teardown(&e);
}

static void test_whois_no_such_nick(void **state) {
	(void)state;
	struct env e;
	setup(&e);
	online(&e);
	assert_true(upstream_enqueue_take(e.uc, 1, "", IRC_MSG("WHOIS", "ghost")));
	assert_true(upstream_enqueue_take(e.uc, 2, "", IRC_MSG("WHOIS", "alice")));
	assert_int_equal(e.sent.len, 1);
	feed(&e, ":server 401 me ghost :No such nick/channel");
	assert_int_equal(e.sent.len, 2);
	feed(&e, ":server 318 me ghost :End of /WHOIS list");
	feed(&e, ":server 311 me alice a host * :Alice");
	feed(&e, ":server 318 me alice :End of /WHOIS list");
	assert_int_equal(e.ids.len, 4);
	assert_int_equal(e.ids.data[0], 1);
	assert_int_equal(e.ids.data[1], 1);
	assert_int_equal(e.ids.data[2], 2);
	assert_int_equal(e.ids.data[3], 2);
	teardown(&e);
}

static void test_who_try_again(void **state) {
	(void)state;
	struct env e;
	setup(&e);
	online(&e);
	assert_true(upstream_enqueue_take(e.uc, 1, "", IRC_MSG("WHO", "#busy")));
	assert_true(upstream_enqueue_take(e.uc, 2, "", IRC_MSG("WHO", "bob")));
	feed(&e, ":server 263 me WHO :Please wait a while and try again");
	feed(&e, ":server 315 me #busy :End of /WHO list");
	feed(&e, ":server 352 me * b host server bob H :0 Bob");
	feed(&e, ":server 315 me bob :End of /WHO list");
	assert_int_equal(e.ids.len, 4);
	assert_int_equal(e.ids.data[0], 1);
	assert_int_equal(e.ids.data[1], 1);
	assert_int_equal(e.ids.data[2], 2);
	assert_int_equal(e.ids.data[3], 2);
	teardown(&e);
}

static void test_labeled_nested_batch(void **state) {
	(void)state;
	struct env e;
	setup(&e);
	online(&e);
	feed(&e, "@label=sd-7-0-l1 :server BATCH +o labeled-response");
	feed(&e, "@batch=o :server BATCH +i example/nested");
	feed(&e, "@batch=i :server 311 me bob b host * :Bob");
	feed(&e, "@batch=i :server 318 me bob :End of WHOIS");
	feed(&e, ":server BATCH -i");
	feed(&e, ":server BATCH -o");
	assert_int_equal(e.received.len, 6);
	assert_string_equal(irc_tags_get(&e.received.data[0]->tags, "label"), "l1");
	assert_null(irc_tags_get(&e.received.data[1]->tags, "label"));
	assert_string_equal(irc_tags_get(&e.received.data[1]->tags, "batch"), "o");
	for (size_t i = 0; i < e.ids.len; i++) {
		assert_int_equal(e.ids.data[i], 7);
	}
	teardown(&e);
}

static void test_auto_detach(void **state) {
	(void)state;
	struct env e;
	setup(&e);
	online(&e);
	feed(&e, ":me!u@host JOIN #chan");
	upstream_set_auto_detach(e.uc, "#chan", 600);
	uint64_t start = e.uc->now;
	for (unsigned i = 1; i <= 4; i++) {
		upstream_tick(e.uc, start + (uint64_t)i * 300);
		assert_int_equal(e.detached, 0);
		feed(&e, ":bob!b@host PRIVMSG #chan :activity");
	}
	upstream_tick(e.uc, start + 1799);
	assert_int_equal(e.detached, 0);
	upstream_tick(e.uc, start + 1800);
	assert_int_equal(e.detached, 1);
	teardown(&e);
}

static void test_command_budget_and_multiline(void **state) {
	(void)state;
	struct env e;
	setup(&e);
	online(&e);
	for (size_t i = 0; i < 64; i++) {
		assert_true(upstream_enqueue_take(e.uc, 1, "", IRC_MSG("WHOIS", "x")));
	}
	assert_false(upstream_enqueue_take(e.uc, 2, "overflow", IRC_MSG("WHOIS", "x")));
	assert_string_equal(vec_last(&e.received)->command, "318");
	assert_int_equal(vec_last(&e.ids), 2);
	upstream_cancel_downstream(e.uc, 1);
	feed(&e, ":server 318 me x :End");
	assert_int_equal(e.sent.len, 1);
	feed(&e, ":bob!b@host BATCH +m draft/multiline #chan");
	feed(&e, "@batch=m :bob!b@host PRIVMSG #chan :one");
	feed(&e, "@batch=m :bob!b@host PRIVMSG #chan :two");
	feed(&e, ":bob!b@host BATCH -m");
	assert_string_equal(irc_msg_param(vec_last(&e.received), 1), "one\ntwo");
	teardown(&e);
}

static void test_tcp_runner_and_pending_close(void **state) {
	(void)state;
	struct env e;
	setup(&e);
	int listener = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
	if (listener < 0) {
		teardown(&e);
		fail_msg("socket failed");
		return;
	}
	struct sockaddr_in addr = {.sin_family = AF_INET,
				   .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
	assert_int_equal(bind(listener, (struct sockaddr *)&addr, sizeof(addr)), 0);
	assert_int_equal(listen(listener, 4), 0);
	socklen_t len = sizeof(addr);
	assert_int_equal(getsockname(listener, (struct sockaddr *)&addr, &len), 0);
	xfree(e.uc->network.addr);
	e.uc->network.addr =
		xasprintf("irc+insecure://127.0.0.1:%u", (unsigned)ntohs(addr.sin_port));
	e.uc->events.send = nullptr;
	upstream_connect(e.uc);
	int peer = -1;
	uint64_t deadline = uv_hrtime() + 5000000000ULL;
	AUTO_STRBUF struct strbuf text = STRBUF_INIT;
	while (uv_hrtime() < deadline && strstr(strbuf_cstr(&text), "USER me") == nullptr) {
		uv_run(&e.loops.loops[0].uv, UV_RUN_NOWAIT);
		if (peer < 0) {
			peer = accept4(listener, nullptr, nullptr, SOCK_NONBLOCK);
		}
		if (peer >= 0) {
			char buf[4096];
			ssize_t n = recv(peer, buf, sizeof(buf), MSG_DONTWAIT);
			if (n > 0) {
				strbuf_append(&text, buf, (size_t)n);
			}
		}
	}
	assert_non_null(strstr(strbuf_cstr(&text), "CAP LS 302\r\n"));
	assert_non_null(strstr(strbuf_cstr(&text), "USER me"));
	if (peer < 0) {
		close(listener);
		teardown(&e);
		fail_msg("upstream did not connect");
		return;
	}
	const char *welcome = ":server 001 me :Welcome\r\n";
	assert_int_equal(send(peer, welcome, strlen(welcome), MSG_NOSIGNAL),
			 (ssize_t)strlen(welcome));
	while (!e.uc->registered && uv_hrtime() < deadline) {
		uv_run(&e.loops.loops[0].uv, UV_RUN_NOWAIT);
	}
	assert_true(e.uc->registered);
	close(peer);
	close(listener);
	teardown(&e);
	// A DNS completion after the session disappears must only free its data.
	setup(&e);
	e.uc->events.send = nullptr;
	upstream_connect(e.uc);
	teardown(&e);
}

T_MAIN(cmocka_unit_test(test_tcp_runner_and_pending_close), cmocka_unit_test(test_errno_messages),
       cmocka_unit_test(test_labels), cmocka_unit_test(test_registration_errors),
       cmocka_unit_test(test_caps_requested), cmocka_unit_test(test_cap_ls_continuation),
       cmocka_unit_test(test_whox_fields), cmocka_unit_test(test_whois_no_such_nick),
       cmocka_unit_test(test_who_try_again), cmocka_unit_test(test_auto_detach),
       cmocka_unit_test(test_labeled_nested_batch), cmocka_unit_test(test_registration_timeout),
       cmocka_unit_test(test_sasl_cap_nak_ends_capability_negotiation),
       cmocka_unit_test(test_command_budget_and_multiline))
