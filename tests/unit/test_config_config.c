// Ported from soju-rs src/config.rs tests
#include "testsupport.h"

#include <stdio.h>

#include "base/alloc.h"
#include "config/config.h"

static const char TURSO[] = "message-store turso\nturso-organization example\nturso-group "
			    "bouncer\nturso-group-token sql-secret\nturso-api-token api-secret\n";

static bool parses(const char *s) {
	struct config c;
	struct err *e = nullptr;
	if (!config_parse(str_from(s), &c, &e)) {
		err_free(e);
		return false;
	}
	config_free(&c);
	return true;
}

static char *parse_error(const char *s) {
	struct config c;
	struct err *e = nullptr;
	assert_false(config_parse(str_from(s), &c, &e));
	char *msg = xstrdup(err_msg(e));
	err_free(e);
	return msg;
}

static void test_turso_config(void **state) {
	(void)state;
	struct config cfg;
	assert_true(config_parse(str_from(TURSO), &cfg, nullptr));
	assert_int_equal(cfg.basic.msg_store_driver, MSG_STORE_TURSO);
	assert_true(cfg.has_turso);
	assert_string_equal(cfg.turso.controller, "soju-controller");
	assert_string_equal(cfg.turso.group_token, "sql-secret");
	// (The Rust test also checks that the Debug output hides the tokens:
	// the C port has no such formatter.)
	config_free(&cfg);
	char buf[512];
	snprintf(buf, sizeof(buf), "%sturso-controller another-controller\n", TURSO);
	assert_true(parses(buf));
	const char *names[] = {"turso-organization", "turso-group", "turso-group-token",
			       "turso-api-token"};
	for (size_t i = 0; i < 4; i++) {
		// The config without the lines starting with the name
		struct strbuf incomplete = STRBUF_INIT;
		struct str rest = str_from(TURSO), line;
		bool first = true;
		while (str_next_field(&rest, '\n', &line)) {
			if (line.len == 0 || str_starts_with(line, str_from(names[i]))) {
				continue;
			}
			if (!first) {
				strbuf_append_char(&incomplete, '\n');
			}
			strbuf_append_str(&incomplete, line);
			first = false;
		}
		char *msg = parse_error(strbuf_cstr(&incomplete));
		assert_non_null(strstr(msg, names[i]));
		xfree(msg);
		strbuf_free(&incomplete);
	}
	const char *suffixes[] = {"db sqlite3 local.db\n", "turso-group duplicate\n",
				  "turso-controller ../escape\n", "turso-controller {}\n"};
	for (size_t i = 0; i < 4; i++) {
		snprintf(buf, sizeof(buf), "%s%s", TURSO, suffixes[i]);
		assert_false(parses(buf));
	}
	assert_false(parses("message-store turso unexpected"));
	assert_false(parses("turso-group unused"));
}

static void test_full_config(void **state) {
	(void)state;
	struct config cfg;
	struct err *e = nullptr;
	bool ok = config_parse(STR_LIT("\n"
				       "listen ircs://\n"
				       "listen irc+insecure://:6667\n"
				       "hostname example.org\n"
				       "title \"My bouncer\"\n"
				       "tls cert.pem key.pem\n"
				       "db sqlite3 /var/lib/soju/main.db\n"
				       "message-store db\n"
				       "auth oauth2 https://id:secret@auth.example.org\n"
				       "http-origin https://*.example.org\n"
				       "accept-proxy-ip localhost 10.0.0.1/8\n"
				       "accept-proxy-unix true\n"
				       "max-user-networks 5\n"
				       "upstream-user-ip 192.168.0.0/24 fd00::/64\n"
				       "disable-inactive-user 30d\n"
				       "message-expiry 0.5d\n"
				       "chathistory-limit 100\n"
				       "enable-user-on-auth true\n"
				       "client-cert-auth true\n"),
			       &cfg, &e);
	if (!ok) {
		fail_msg("%s", err_msg(e));
	}
	assert_int_equal(cfg.listen.len, 2);
	assert_string_equal(cfg.listen.data[0], "ircs://");
	assert_string_equal(cfg.listen.data[1], "irc+insecure://:6667");
	assert_string_equal(cfg.basic.hostname, "example.org");
	assert_string_equal(cfg.basic.title, "My bouncer");
	assert_string_equal(cfg.basic.http_ingress, "https://example.org");
	assert_string_equal(cfg.db_source, "/var/lib/soju/main.db");
	assert_int_equal(cfg.basic.msg_store_driver, MSG_STORE_DB);
	assert_int_equal(cfg.auth.len, 1);
	assert_int_equal(cfg.auth.data[0].driver, AUTH_OAUTH2);
	struct ipaddr ip;
	assert_true(ipaddr_parse(STR_LIT("127.0.0.1"), &ip));
	assert_true(ipset_contains(&cfg.basic.accept_proxy_ips, ip));
	assert_true(ipaddr_parse(STR_LIT("::ffff:10.1.2.3"), &ip));
	assert_true(ipset_contains(&cfg.basic.accept_proxy_ips, ip));
	assert_true(ipaddr_parse(STR_LIT("11.0.0.1"), &ip));
	assert_false(ipset_contains(&cfg.basic.accept_proxy_ips, ip));
	assert_true(cfg.basic.accept_proxy_unix);
	assert_int_equal(cfg.basic.max_user_networks, 5);
	assert_int_equal(cfg.basic.chat_history_limit, 100);
	assert_int_equal(cfg.basic.upstream_user_ips.len, 2);
	assert_int_equal(cfg.basic.disable_inactive_users_delay, 30ULL * 86400 * 1000000000);
	assert_int_equal(cfg.basic.message_expiry, 43200ULL * 1000000000);
	assert_true(cfg.basic.enable_users_on_auth);
	assert_true(cfg.basic.client_cert_auth);
	config_free(&cfg);
}

static void test_defaults(void **state) {
	(void)state;
	struct config cfg;
	assert_true(config_parse(STR_LIT(""), &cfg, nullptr));
	assert_string_equal(cfg.db_driver, "sqlite3");
	assert_string_equal(cfg.db_source, "soju.db");
	assert_int_equal(cfg.basic.msg_store_driver, MSG_STORE_DB);
	assert_int_equal(cfg.basic.max_user_networks, -1);
	assert_int_equal(cfg.basic.chat_history_limit, 1000);
	assert_int_equal(cfg.auth.data[0].driver, AUTH_INTERNAL);
	config_free(&cfg);
}

static void test_errors(void **state) {
	(void)state;
	assert_false(parses("unknown x"));
	assert_false(parses("hostname a\nhostname b"));
	assert_false(parses("tls a"));
	assert_false(parses("db postgres x"));
	assert_false(parses("message-store db x"));
	assert_false(parses("auth http"));
	assert_false(parses("disable-inactive-user 30"));
	assert_false(parses("upstream-user-ip 1.2.3.0/24 4.5.6.0/24"));
	assert_false(parses("max-user-networks x"));
	const char *bad[] = {"0", "99", "1001", "-1", "x"};
	char buf[64];
	for (size_t i = 0; i < 5; i++) {
		snprintf(buf, sizeof(buf), "chathistory-limit %s", bad[i]);
		assert_false(parses(buf));
	}
	assert_false(parses("enable-user-on-auth maybe"));
}

static void dur_err(const char *s, const char *want) {
	duration_ns d;
	struct err *e = nullptr;
	assert_false(config_parse_duration(str_from(s), &d, &e));
	assert_string_equal(err_msg(e), want);
	err_free(e);
}

static void test_durations(void **state) {
	(void)state;
	duration_ns d;
	assert_true(config_parse_duration(STR_LIT("0d"), &d, nullptr));
	assert_int_equal(d, 0);
	assert_true(config_parse_duration(STR_LIT("1.5d"), &d, nullptr));
	assert_int_equal(d, 129600ULL * 1000000000);
	assert_false(config_parse_duration(STR_LIT("5"), &d, nullptr));
	const char *bad[] = {"-1d", "infd", "nand", "1e20d", "1e9d"};
	for (size_t i = 0; i < 5; i++) {
		dur_err(bad[i], "duration must be positive");
	}
}

static void test_log_alias(void **state) {
	(void)state;
	struct config cfg;
	assert_true(config_parse(STR_LIT("log memory"), &cfg, nullptr));
	assert_int_equal(cfg.basic.msg_store_driver, MSG_STORE_MEMORY);
	config_free(&cfg);
}

static void test_fs_message_store_unsupported(void **state) {
	(void)state;
	const char *cs[] = {"message-store fs /tmp/logs", "log fs /tmp/logs", "message-store fs"};
	for (size_t i = 0; i < 3; i++) {
		char *msg = parse_error(cs[i]);
		assert_string_equal(msg, "directive message-store: driver \"fs\" is not supported");
		xfree(msg);
	}
}

// C-only: messages of the errors soju-rs formats itself
static void test_error_messages(void **state) {
	(void)state;
	const struct {
		const char *config, *want;
	} cases[] = {
		{"unknown x", "line 1, directive \"unknown\": unknown directive"},
		{"hostname a\nhostname b",
		 "line 2, directive \"hostname\": directive must not be specified more than once"},
		{"tls a", "line 1, directive \"tls\": directive requires exactly 2 parameters"},
		{"max-user-networks x",
		 "line 1, directive \"max-user-networks\", parameter 1: invalid int parameter: "
		 "strconv.ParseInt: parsing \"x\": invalid syntax"},
		{"accept-proxy-ip 1.2.3/8",
		 "directive accept-proxy-ip: failed to parse CIDR: invalid CIDR address: 1.2.3/8"},
		{"enable-user-on-auth maybe", "directive enable-user-on-auth: strconv.ParseBool: "
					      "parsing \"maybe\": invalid syntax"},
		{"disable-inactive-user xd",
		 "directive disable-inactive-user: invalid duration: invalid float literal"},
		{"message-expiry d", "directive message-expiry: invalid duration: cannot parse "
				     "float from empty string"},
		{"chathistory-limit 5",
		 "directive chathistory-limit: must be an integer between 100 and 1000"},
		{"auth pam x", "directive auth: driver \"pam\" requires zero parameters"},
		{"auth foo", "directive auth: unknown driver \"foo\""},
		{"http-origin [", "directive http-origin: syntax error in pattern"},
		{"message-store turso\nturso-organization Bad\nturso-group g\nturso-group-token "
		 "t\nturso-api-token a",
		 "turso-organization must contain 1 to 63 lowercase letters, digits or hyphens"},
	};
	for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
		char *msg = parse_error(cases[i].config);
		assert_string_equal(msg, cases[i].want);
		xfree(msg);
	}
	struct config cfg;
	assert_true(config_parse(STR_LIT("upstream-user-ip 010.0.0.1/08"), &cfg, nullptr));
	assert_int_equal(cfg.basic.upstream_user_ips.data[0].prefix, 8);
	assert_int_equal(cfg.basic.upstream_user_ips.data[0].addr.b[0], 10);
	assert_int_equal(cfg.basic.upstream_user_ips.data[0].addr.b[3], 0);
	config_free(&cfg);
}

T_MAIN(cmocka_unit_test(test_turso_config), cmocka_unit_test(test_full_config),
       cmocka_unit_test(test_defaults), cmocka_unit_test(test_errors),
       cmocka_unit_test(test_durations), cmocka_unit_test(test_log_alias),
       cmocka_unit_test(test_fs_message_store_unsupported), cmocka_unit_test(test_error_messages))
