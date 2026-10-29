// Ported from soju-rs src/database/mod.rs tests (network_url, host_port)
#include "testsupport.h"

#include "base/alloc.h"
#include "net/url.h"

static void test_network_url(void **state) {
	(void)state;
	struct url u;
	assert_true(network_url("irc.libera.chat", &u, nullptr));
	assert_string_equal(u.scheme, "ircs");
	assert_string_equal(u.host, "irc.libera.chat");
	url_free(&u);
	assert_true(network_url("irc+unix:///run/ircd.sock", &u, nullptr));
	assert_string_equal(u.host, "");
	assert_string_equal(u.path, "/run/ircd.sock");
	url_free(&u);
	assert_true(network_url("unix://foo/bar", &u, nullptr));
	assert_string_equal(u.path, "foo/bar");
	url_free(&u);
	assert_true(network_url("ircs://user:pass@host:6697?x#y", &u, nullptr));
	assert_true(u.has_user);
	assert_string_equal(u.host, "host:6697");
	assert_string_equal(u.query, "x");
	assert_string_equal(u.fragment, "y");
	url_free(&u);
}

static void hp_eq(const char *in, const char *host, const char *port) {
	char *h, *p;
	assert_true(split_host_port(str_from(in), &h, &p, nullptr));
	assert_string_equal(h, host);
	assert_string_equal(p, port);
	xfree(h);
	xfree(p);
}

static void hp_err(const char *in, const char *want) {
	char *h, *p;
	struct err *e = nullptr;
	assert_false(split_host_port(str_from(in), &h, &p, &e));
	assert_string_equal(err_msg(e), want);
	err_free(e);
}

static void test_host_port(void **state) {
	(void)state;
	hp_eq("a:1", "a", "1");
	hp_eq("[::1]:6697", "::1", "6697");
	hp_err("a", "address a: missing port in address");
	hp_err("::1", "address ::1: too many colons in address");
	char *s = join_host_port("::1", "1");
	assert_string_equal(s, "[::1]:1");
	xfree(s);
}

// C-only: URL errors
static void test_url_errors(void **state) {
	(void)state;
	struct url u;
	struct err *e = nullptr;
	assert_false(url_parse(STR_LIT("host"), &u, &e));
	assert_string_equal(err_msg(e), "missing scheme");
	err_free(e);
	e = nullptr;
	assert_false(url_parse(STR_LIT("1x://h"), &u, &e));
	assert_string_equal(err_msg(e), "first path segment in URL cannot contain colon");
	err_free(e);
	e = nullptr;
	assert_false(network_url("ircs://a b", &u, &e));
	assert_string_equal(
		err_msg(e),
		"failed to parse upstream server URL: invalid character \" \" in host name");
	err_free(e);
}

T_MAIN(cmocka_unit_test(test_network_url), cmocka_unit_test(test_host_port),
       cmocka_unit_test(test_url_errors))
