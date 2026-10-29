// Ported from soju-rs src/listen.rs tests (uris)
#include "testsupport.h"

#include "config/config.h"
#include "net/listen_uri.h"

static void check(const char *uri, enum listener_kind kind, bool unix, const char *addr, bool tls) {
	struct listener_spec l;
	struct err *e = nullptr;
	if (!parse_listen_uri(uri, &l, &e)) {
		fail_msg("%s: %s", uri, err_msg(e));
	}
	assert_int_equal(l.kind, kind);
	assert_int_equal(l.unix, unix);
	assert_string_equal(l.addr, addr);
	assert_int_equal(l.tls, tls);
	assert_string_equal(l.label, uri);
	listener_spec_free(&l);
}

static bool ok(const char *uri) {
	struct listener_spec l;
	struct err *e = nullptr;
	if (!parse_listen_uri(uri, &l, &e)) {
		err_free(e);
		return false;
	}
	listener_spec_free(&l);
	return true;
}

static void test_uris(void **state) {
	(void)state;
	check("ircs://", LISTENER_IRC, false, ":6697", true);
	check(":6697", LISTENER_IRC, false, ":6697", true);
	check("irc+insecure://0.0.0.0:6667", LISTENER_IRC, false, "0.0.0.0:6667", false);
	assert_false(ok("irc://0.0.0.0"));
	assert_true(ok("irc://localhost"));
	check("unix:///run/soju/irc.sock", LISTENER_IRC, true, "/run/soju/irc.sock", false);
	check("unix+admin://", LISTENER_ADMIN, true, CONFIG_DEFAULT_UNIX_ADMIN_PATH, false);
	check("ws+insecure://:8080", LISTENER_HTTP, false, ":8080", false);
	check("wss://", LISTENER_HTTP, false, ":443", true);
	assert_false(ok("ws://example.org"));
	assert_true(ok("http+prometheus://localhost:9090"));
	assert_false(ok("http+prometheus://0.0.0.0:9090"));
	assert_false(ok("http+prometheus://localhost"));
	assert_false(ok("gopher://"));
	check("ident://", LISTENER_IDENT, false, ":113", false);
}

T_MAIN(cmocka_unit_test(test_uris))
