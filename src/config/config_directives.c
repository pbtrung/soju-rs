// The directives of the configuration file (soju-rs config.rs parse):
// each group of directives is parsed by a handler, in a fixed order which
// decides the error reported first
#include "config/internal.h"

#include <errno.h>
#include <stdlib.h>

#include "base/alloc.h"
#include "base/path_match.h"
#include "base/utf8.h"
#include "config/flags.h"
#include "config/scfg.h"

static const char *const KNOWN_DIRECTIVES[] = {
	"listen",
	"hostname",
	"title",
	"icon",
	"motd",
	"tls",
	"db",
	"message-store",
	"log",
	"auth",
	"file-upload",
	"http-origin",
	"http-ingress",
	"accept-proxy-ip",
	"accept-proxy-unix",
	"max-user-networks",
	"upstream-user-ip",
	"disable-inactive-user",
	"message-expiry",
	"chathistory-limit",
	"enable-user-on-auth",
	"client-cert-auth",
	"turso-organization",
	"turso-group",
	"turso-controller",
	"turso-group-token",
	"turso-api-token",
};

static constexpr size_t N_KNOWN = sizeof(KNOWN_DIRECTIVES) / sizeof(KNOWN_DIRECTIVES[0]);

#define TRY(x)                    \
	do {                      \
		if (!(x)) {       \
			goto out; \
		}                 \
	} while (0)
#define FAIL(...)                             \
	do {                                  \
		err_set(err, 0, __VA_ARGS__); \
		goto out;                     \
	} while (0)

// The known directives
static bool check_known(const struct scfg_block *block, struct config *srv, struct err **err) {
	(void)srv;
	bool ok = false;
	for (size_t i = 0; i < block->v.len; i++) {
		const struct scfg_directive *d = &block->v.data[i];
		bool known = false;
		for (size_t k = 0; k < N_KNOWN; k++) {
			known = known || cstr_eq(d->name, KNOWN_DIRECTIVES[k]);
		}
		if (!known) {
			cfg_dir_err(err, d, "unknown directive");
			goto out;
		}
	}
	ok = true;
out:
	return ok;
}

// Listeners, server name, MOTD and TLS
static bool parse_basic(const struct scfg_block *block, struct config *srv, struct err **err) {
	dirvec dirs = {0};
	bool ok = false;
	const char *s, *a, *b;
	cfg_get(block, "listen", &dirs);
	for (size_t i = 0; i < dirs.len; i++) {
		const struct scfg_directive *d = dirs.data[i];
		if (d->has_children) {
			cfg_dir_err(err, d, "directive requires zero children");
			goto out;
		}
		if (d->params.len != 1) {
			cfg_dir_err(err, d, "directive requires exactly one parameter");
			goto out;
		}
		strvec_push(&srv->listen, d->params.data[0]);
	}
	cfg_get(block, "hostname", &dirs);
	TRY(cfg_single_string(&dirs, &s, err));
	if (s != nullptr && s[0] != '\0') {
		cfg_replace(&srv->basic.hostname, s);
	}
	cfg_get(block, "title", &dirs);
	TRY(cfg_single_string(&dirs, &s, err));
	cfg_replace(&srv->basic.title, s != nullptr ? s : "");
	cfg_get(block, "icon", &dirs);
	TRY(cfg_single_string(&dirs, &s, err));
	cfg_replace(&srv->icon, s != nullptr ? s : "");
	cfg_get(block, "motd", &dirs);
	TRY(cfg_single_string(&dirs, &s, err));
	cfg_replace(&srv->motd_path, s != nullptr ? s : "");
	cfg_get(block, "tls", &dirs);
	TRY(cfg_single_pair(&dirs, &a, &b, err));
	if (a != nullptr) {
		srv->has_tls = true;
		srv->tls_cert_path = xstrdup(a);
		srv->tls_key_path = xstrdup(b);
	}
	ok = true;
out:
	vec_free(&dirs);
	return ok;
}

// Database and message store
static bool parse_storage(const struct scfg_block *block, struct config *srv, struct err **err) {
	dirvec dirs = {0};
	bool ok = false;
	const char *a, *b;
	const strvec *list;
	cfg_get(block, "db", &dirs);
	TRY(cfg_single_pair(&dirs, &a, &b, err));
	if (a != nullptr) {
		if (cstr_eq(a, "postgres")) {
			FAIL("directive db: driver \"postgres\" is not supported");
		} else if (!cstr_eq(a, "sqlite3")) {
			AUTOFREE char *q = utf8_debug_cstr(str_from(a));
			FAIL("directive db: unknown driver %s", q);
		}
		cfg_replace(&srv->db_driver, a);
		cfg_replace(&srv->db_source, b);
	}

	cfg_get(block, "message-store", &dirs);
	TRY(cfg_single_list(&dirs, &list, err));
	if (list == nullptr) {
		cfg_get(block, "log", &dirs);
		TRY(cfg_single_list(&dirs, &list, err));
	}
	if (list != nullptr) {
		TRY(cfg_parse_driver_source("message-store", list, &a, &b, err));
		enum msg_store_driver driver;
		if (cstr_eq(a, "memory")) {
			driver = MSG_STORE_MEMORY;
		} else if (cstr_eq(a, "db")) {
			driver = MSG_STORE_DB;
		} else if (cstr_eq(a, "turso")) {
			driver = MSG_STORE_TURSO;
		} else if (cstr_eq(a, "fs")) {
			FAIL("directive message-store: driver \"fs\" is not supported");
		} else {
			AUTOFREE char *q = utf8_debug_cstr(str_from(a));
			FAIL("directive message-store: unknown driver %s", q);
		}
		if (b[0] != '\0') {
			AUTOFREE char *q = utf8_debug_cstr(str_from(list->data[0]));
			FAIL("directive message-store: driver %s requires zero parameters", q);
		}
		srv->basic.msg_store_driver = driver;
		cfg_replace(&srv->basic.msg_store_source, b);
	}
	ok = true;
out:
	vec_free(&dirs);
	return ok;
}

// Turso
static bool parse_turso(const struct scfg_block *block, struct config *srv, struct err **err) {
	dirvec dirs = {0};
	bool ok = false;
	if (srv->basic.msg_store_driver == MSG_STORE_TURSO) {
		cfg_get(block, "db", &dirs);
		if (dirs.len > 0) {
			FAIL("message-store turso manages its own databases; remove the db "
			     "directive");
		}
		// In the order soju-rs checks them
		static const char *const names_in_order[] = {
			"turso-organization", "turso-group", "turso-controller",
			"turso-group-token", "turso-api-token"};
		const char *vals[5];
		for (size_t k = 0; k < 5; k++) {
			cfg_get(block, names_in_order[k], &dirs);
			TRY(cfg_single_string(&dirs, &vals[k], err));
			if (k == 2) {
				if (vals[k] == nullptr) {
					vals[k] = "soju-controller";
				}
			} else if (vals[k] == nullptr || vals[k][0] == '\0') {
				FAIL("message-store turso requires %s", names_in_order[k]);
			}
		}
		struct config_turso t = {
			.organization = xstrdup(vals[0]),
			.group = xstrdup(vals[1]),
			.controller = xstrdup(vals[2]),
			.group_token = xstrdup(vals[3]),
			.api_token = xstrdup(vals[4]),
		};
		const char *names[] = {"turso-organization", "turso-group", "turso-controller"};
		const char *values[] = {t.organization, t.group, t.controller};
		for (size_t k = 0; k < 3; k++) {
			size_t l = strlen(values[k]);
			bool valid = l > 0 && l <= 63;
			for (size_t j = 0; j < l; j++) {
				char c = values[k][j];
				valid = valid && ((c >= 'a' && c <= 'z') ||
						  (c >= '0' && c <= '9') || c == '-');
			}
			if (!valid) {
				config_turso_free(&t);
				FAIL("%s must contain 1 to 63 lowercase letters, digits or hyphens",
				     names[k]);
			}
		}
		srv->turso = t;
		srv->has_turso = true;
	} else {
		for (size_t k = 0; k < N_KNOWN; k++) {
			if (strncmp(KNOWN_DIRECTIVES[k], "turso-", 6) != 0) {
				continue;
			}
			cfg_get(block, KNOWN_DIRECTIVES[k], &dirs);
			if (dirs.len > 0) {
				FAIL("turso directives require message-store turso");
			}
		}
	}
	ok = true;
out:
	vec_free(&dirs);
	return ok;
}

// Authentication
static bool parse_auth(const struct scfg_block *block, struct config *srv, struct err **err) {
	dirvec dirs = {0};
	bool ok = false;
	const char *a, *b;
	cfg_get(block, "auth", &dirs);
	if (dirs.len > 0) {
		// drop default auth if we are explicitly defining any auth
		vec_free_with(&srv->auth, config_free_auth);
	}
	for (size_t i = 0; i < dirs.len; i++) {
		const struct scfg_directive *d = dirs.data[i];
		if (d->has_children) {
			cfg_dir_err(err, d, "directive requires zero children");
			goto out;
		}
		TRY(cfg_parse_driver_source("auth", &d->params, &a, &b, err));
		enum auth_driver drv;
		AUTOFREE char *q = utf8_debug_cstr(str_from(a));
		if (cstr_eq(a, "internal")) {
			drv = AUTH_INTERNAL;
		} else if (cstr_eq(a, "pam")) {
			drv = AUTH_PAM;
		} else if (cstr_eq(a, "http")) {
			drv = AUTH_HTTP;
		} else if (cstr_eq(a, "oauth2")) {
			drv = AUTH_OAUTH2;
		} else {
			FAIL("directive auth: unknown driver %s", q);
		}
		if ((drv == AUTH_INTERNAL || drv == AUTH_PAM) && b[0] != '\0') {
			FAIL("directive auth: driver %s requires zero parameters", q);
		}
		if ((drv == AUTH_HTTP || drv == AUTH_OAUTH2) && b[0] == '\0') {
			FAIL("directive auth: driver %s requires a source", q);
		}
		vec_push(&srv->auth, (struct config_auth){drv, xstrdup(b)});
	}
	ok = true;
out:
	vec_free(&dirs);
	return ok;
}

// File uploads, HTTP and proxies
static bool parse_http(const struct scfg_block *block, struct config *srv, struct err **err) {
	dirvec dirs = {0};
	bool ok = false;
	struct err *e = nullptr;
	const char *s, *a, *b;
	const strvec *list;
	cfg_get(block, "file-upload", &dirs);
	TRY(cfg_single_list(&dirs, &list, err));
	if (list != nullptr) {
		TRY(cfg_parse_driver_source("file-upload", list, &a, &b, err));
		srv->file_upload = true;
	}

	cfg_get(block, "http-origin", &dirs);
	TRY(cfg_single_list(&dirs, &list, err));
	if (list != nullptr) {
		for (size_t i = 0; i < list->len; i++) {
			bool m;
			if (!path_match(str_from(list->data[i]), str_from(list->data[i]), &m)) {
				FAIL("directive http-origin: syntax error in pattern");
			}
		}
		strvec_copy(&srv->basic.http_origins, list);
	}

	cfg_get(block, "http-ingress", &dirs);
	TRY(cfg_single_string(&dirs, &s, err));
	if (s != nullptr && s[0] != '\0') {
		cfg_replace(&srv->basic.http_ingress, s);
	} else {
		char *v = xasprintf("https://%s", srv->basic.hostname);
		xfree(srv->basic.http_ingress);
		srv->basic.http_ingress = v;
	}

	cfg_get(block, "accept-proxy-ip", &dirs);
	TRY(cfg_single_list(&dirs, &list, err));
	for (size_t i = 0; list != nullptr && i < list->len; i++) {
		if (cstr_eq(list->data[i], "localhost")) {
			cfg_push_loopback(&srv->basic.accept_proxy_ips);
			continue;
		}
		struct ipnet n;
		if (!parse_cidr(str_from(list->data[i]), &n, &e)) {
			cfg_wrapped(err, e, "directive accept-proxy-ip: failed to parse CIDR");
			goto out;
		}
		vec_push(&srv->basic.accept_proxy_ips.v, n);
	}
	cfg_get(block, "accept-proxy-unix", &dirs);
	TRY(cfg_single_string(&dirs, &s, err));
	if (s != nullptr && s[0] != '\0') {
		if (!cfg_parse_bool(s, &srv->basic.accept_proxy_unix, &e)) {
			cfg_wrapped(err, e, "directive accept-proxy-unix");
			goto out;
		}
	}
	ok = true;
out:
	vec_free(&dirs);
	return ok;
}

// Network limits and upstream source IPs
static bool parse_limits(const struct scfg_block *block, struct config *srv, struct err **err) {
	dirvec dirs = {0};
	bool ok = false;
	struct err *e = nullptr;
	const char *s;
	const strvec *list;
	cfg_get(block, "max-user-networks", &dirs);
	TRY(cfg_single_string(&dirs, &s, err));
	if (s != nullptr) {
		long long n;
		if (!str_parse_i64(str_from(s), &n)) {
			AUTOFREE char *qn = utf8_debug_cstr(str_from(dirs.data[0]->name));
			AUTOFREE char *qs = utf8_debug_cstr(str_from(s));
			FAIL("line %zu, directive %s, parameter 1: invalid int parameter: "
			     "strconv.ParseInt: parsing %s: invalid syntax",
			     dirs.data[0]->lineno, qn, qs);
		}
		srv->basic.max_user_networks = n;
	}
	cfg_get(block, "upstream-user-ip", &dirs);
	TRY(cfg_single_list(&dirs, &list, err));
	bool has_v4 = false, has_v6 = false;
	for (size_t i = 0; list != nullptr && i < list->len; i++) {
		struct ipnet n;
		if (!parse_cidr(str_from(list->data[i]), &n, &e)) {
			cfg_wrapped(err, e, "directive upstream-user-ip: failed to parse CIDR");
			goto out;
		}
		if (!n.addr.v6) {
			if (has_v4) {
				FAIL("directive upstream-user-ip: found two IPv4 CIDRs");
			}
			has_v4 = true;
		} else {
			if (has_v6) {
				FAIL("directive upstream-user-ip: found two IPv6 CIDRs");
			}
			has_v6 = true;
		}
		vec_push(&srv->basic.upstream_user_ips, n);
	}
	ok = true;
out:
	vec_free(&dirs);
	return ok;
}

// User and history settings
static bool parse_users(const struct scfg_block *block, struct config *srv, struct err **err) {
	dirvec dirs = {0};
	bool ok = false;
	struct err *e = nullptr;
	const char *s;
	cfg_get(block, "disable-inactive-user", &dirs);
	TRY(cfg_single_string(&dirs, &s, err));
	if (s != nullptr && s[0] != '\0') {
		if (!config_parse_duration(str_from(s), &srv->basic.disable_inactive_users_delay,
					   &e)) {
			cfg_wrapped(err, e, "directive disable-inactive-user");
			goto out;
		}
	}
	cfg_get(block, "message-expiry", &dirs);
	TRY(cfg_single_string(&dirs, &s, err));
	if (s != nullptr && s[0] != '\0') {
		if (!config_parse_duration(str_from(s), &srv->basic.message_expiry, &e)) {
			cfg_wrapped(err, e, "directive message-expiry");
			goto out;
		}
	}
	cfg_get(block, "chathistory-limit", &dirs);
	TRY(cfg_single_string(&dirs, &s, err));
	if (s != nullptr) {
		unsigned long long n;
		if (!str_parse_u64(str_from(s), &n) || n < MIN_CHAT_HISTORY_LIMIT ||
		    n > MAX_CHAT_HISTORY_LIMIT) {
			FAIL("directive chathistory-limit: must be an integer between %zu and %zu",
			     MIN_CHAT_HISTORY_LIMIT, MAX_CHAT_HISTORY_LIMIT);
		}
		srv->basic.chat_history_limit = (size_t)n;
	}
	cfg_get(block, "enable-user-on-auth", &dirs);
	TRY(cfg_single_string(&dirs, &s, err));
	if (s != nullptr && s[0] != '\0') {
		if (!cfg_parse_bool(s, &srv->basic.enable_users_on_auth, &e)) {
			cfg_wrapped(err, e, "directive enable-user-on-auth");
			goto out;
		}
	}
	cfg_get(block, "client-cert-auth", &dirs);
	TRY(cfg_single_string(&dirs, &s, err));
	if (s != nullptr && s[0] != '\0') {
		if (!cfg_parse_bool(s, &srv->basic.client_cert_auth, &e)) {
			cfg_wrapped(err, e, "directive client-cert-auth");
			goto out;
		}
	}
	ok = true;
out:
	vec_free(&dirs);
	return ok;
}

#undef TRY
#undef FAIL

// In order: an error stops the parsing
static bool (*const handlers[])(const struct scfg_block *block, struct config *srv,
				struct err **err) = {
	check_known, parse_basic, parse_storage, parse_turso,
	parse_auth,  parse_http,  parse_limits,  parse_users,
};

bool config_parse_block(const struct scfg_block *block, struct config *srv, struct err **err) {
	for (size_t i = 0; i < sizeof(handlers) / sizeof(*handlers); i++) {
		if (!handlers[i](block, srv, err)) {
			return false;
		}
	}
	return true;
}
