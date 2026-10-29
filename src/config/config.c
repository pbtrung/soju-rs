#include "config/config.h"

#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <unistd.h>

#include "base/alloc.h"
#include "base/utf8.h"
#include "config/flags.h"
#include "config/scfg.h"
#include "base/path_match.h"

bool ipset_contains(const struct ipset *s, struct ipaddr ip) {
	ip = ipaddr_canonical(ip);
	for (size_t i = 0; i < s->v.len; i++) {
		if (ipnet_contains(s->v.data[i], ip)) {
			return true;
		}
	}
	return false;
}

// The loopback networks 127.0.0.0/8 and ::1/128
static void push_loopback(struct ipset *s) {
	struct ipnet n;
	(void)ipnet_parse(STR_LIT("127.0.0.0/8"), &n);
	vec_push(&s->v, n);
	(void)ipnet_parse(STR_LIT("::1/128"), &n);
	vec_push(&s->v, n);
}

bool parse_rust_f64(struct str s, double *out) {
	size_t i = 0;
	if (i < s.len && (s.p[i] == '+' || s.p[i] == '-')) {
		i++;
	}
	struct str rest = str_slice(s, i, s.len);
	bool special = str_eq_ignore_ascii_case(rest, STR_LIT("inf")) ||
		       str_eq_ignore_ascii_case(rest, STR_LIT("infinity")) ||
		       str_eq_ignore_ascii_case(rest, STR_LIT("nan"));
	if (!special) {
		size_t digits = 0;
		while (i < s.len && s.p[i] >= '0' && s.p[i] <= '9') {
			i++;
			digits++;
		}
		if (i < s.len && s.p[i] == '.') {
			i++;
			while (i < s.len && s.p[i] >= '0' && s.p[i] <= '9') {
				i++;
				digits++;
			}
		}
		if (digits == 0) {
			return false;
		}
		if (i < s.len && (s.p[i] == 'e' || s.p[i] == 'E')) {
			i++;
			if (i < s.len && (s.p[i] == '+' || s.p[i] == '-')) {
				i++;
			}
			size_t exp_digits = 0;
			while (i < s.len && s.p[i] >= '0' && s.p[i] <= '9') {
				i++;
				exp_digits++;
			}
			if (exp_digits == 0) {
				return false;
			}
		}
		if (i != s.len) {
			return false;
		}
	}
	AUTOFREE char *c = str_dup(s);
	*out = strtod(c, nullptr);
	return true;
}

__extension__ typedef unsigned __int128 u128;

// Duration::try_from_secs_f64 for 0 <= secs <= MAX_GO_DURATION_SECS: the
// exact value of the double, rounded to the nearest nanosecond (ties to
// even)
static uint64_t secs_to_nanos(double secs) {
	if (secs == 0) {
		return 0;
	}
	int e;
	double m = frexp(secs, &e); // secs = m * 2^e, 0.5 <= m < 1
	uint64_t mant = (uint64_t)ldexp(m, 53);
	e -= 53; // secs = mant * 2^e
	u128 n = (u128)mant * 1000000000U;
	if (e >= 0) {
		return (uint64_t)(n << (unsigned)e);
	}
	unsigned sh = (unsigned)-e;
	if (sh >= 120) {
		return 0;
	}
	u128 q = n >> sh;
	u128 r = n - (q << sh);
	u128 half = (u128)1 << (sh - 1U);
	if (r > half || (r == half && (q & 1U) != 0)) {
		q++;
	}
	return (uint64_t)q;
}

bool config_parse_duration(struct str s, duration_ns *out, struct err **err) {
	if (s.len == 0 || s.p[s.len - 1] != 'd') {
		return err_set(err, 0, "missing 'd' suffix in duration");
	}
	struct str num = str_slice(s, 0, s.len - 1);
	double v;
	if (!parse_rust_f64(num, &v)) {
		return err_set(err, 0, "invalid duration: %s",
			       num.len == 0 ? "cannot parse float from empty string"
					    : "invalid float literal");
	}
	// Upstream's conversion to a time.Duration (int64 nanoseconds)
	// overflows to a negative duration for values out of range (including
	// inf and NaN)
	double secs = v * 24.0 * 3600.0;
	if (!(v >= 0.0) || !(secs <= MAX_GO_DURATION_SECS)) {
		return err_set(err, 0, "duration must be positive");
	}
	*out = secs_to_nanos(secs);
	return true;
}

char *system_hostname(void) {
	char buf[256];
	if (gethostname(buf, sizeof(buf)) == 0) {
		buf[sizeof(buf) - 1] = '\0';
		// gethostname::gethostname().into_string(): non-UTF-8 is an error
		if (buf[0] != '\0' && utf8_valid(str_from(buf))) {
			return xstrdup(buf);
		}
	}
	return xstrdup("localhost");
}

void config_defaults(struct config *c) {
	char *hostname = system_hostname();
	*c = (struct config){
		.basic =
			{
				.http_ingress = xasprintf("https://%s", hostname),
				.hostname = hostname,
				.title = xstrdup(""),
				.msg_store_driver = MSG_STORE_DB,
				.msg_store_source = xstrdup(""),
				.max_user_networks = -1,
				.chat_history_limit = MAX_CHAT_HISTORY_LIMIT,
			},
		.icon = xstrdup(""),
		.motd_path = xstrdup(""),
		.db_driver = xstrdup("sqlite3"),
		.db_source = xstrdup("soju.db"),
	};
	vec_push(&c->auth, (struct config_auth){AUTH_INTERNAL, xstrdup("")});
}

static void free_auth(struct config_auth *a) {
	xfree(a->source);
}

static void turso_free(struct config_turso *t) {
	xfree(t->organization);
	xfree(t->group);
	xfree(t->controller);
	xfree(t->group_token);
	xfree(t->api_token);
}

void config_free(struct config *c) {
	struct config_basic *b = &c->basic;
	xfree(b->hostname);
	xfree(b->title);
	xfree(b->msg_store_source);
	strvec_free(&b->http_origins);
	xfree(b->http_ingress);
	vec_free(&b->accept_proxy_ips.v);
	vec_free(&b->upstream_user_ips);
	strvec_free(&c->listen);
	xfree(c->tls_cert_path);
	xfree(c->tls_key_path);
	xfree(c->icon);
	xfree(c->motd_path);
	xfree(c->db_driver);
	xfree(c->db_source);
	vec_free_with(&c->auth, free_auth);
	turso_free(&c->turso);
	*c = (struct config){0};
}

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

typedef VEC(const struct scfg_directive *) dirvec;

static void get(const struct scfg_block *b, const char *name, dirvec *out) {
	vec_clear(out);
	for (size_t i = 0; i < b->v.len; i++) {
		if (cstr_eq(b->v.data[i].name, name)) {
			vec_push(out, &b->v.data[i]);
		}
	}
}

[[gnu::format(printf, 3, 4)]] static bool dir_err(struct err **err, const struct scfg_directive *d,
						  const char *fmt, ...) {
	va_list ap;
	va_start(ap, fmt);
	AUTO_STRBUF struct strbuf msg = STRBUF_INIT;
	strbuf_vprintf(&msg, fmt, ap);
	va_end(ap);
	AUTOFREE char *q = utf8_debug_cstr(str_from(d->name));
	return err_set(err, 0, "line %zu, directive %s: %s", d->lineno, q, strbuf_cstr(&msg));
}

// The single directive with this name, if any. Mirrors go-scfg which
// rejects non-repeatable directives specified more than once.
static bool single(const dirvec *dirs, const struct scfg_directive **out, struct err **err) {
	if (dirs->len > 1) {
		return dir_err(err, dirs->data[1],
			       "directive must not be specified more than once");
	}
	const struct scfg_directive *d = dirs->len > 0 ? dirs->data[0] : nullptr;
	if (d != nullptr && d->has_children) {
		return dir_err(err, d, "directive requires zero children");
	}
	*out = d;
	return true;
}

// *out is borrowed from the directive, nullptr if missing
static bool single_string(const dirvec *dirs, const char **out, struct err **err) {
	const struct scfg_directive *d = nullptr;
	if (!single(dirs, &d, err)) {
		return false;
	}
	*out = nullptr;
	if (d != nullptr) {
		if (d->params.len != 1) {
			return dir_err(err, d, "directive requires exactly one parameter");
		}
		*out = d->params.data[0];
	}
	return true;
}

// *out: the parameters (borrowed), nullptr if missing
static bool single_list(const dirvec *dirs, const strvec **out, struct err **err) {
	const struct scfg_directive *d = nullptr;
	if (!single(dirs, &d, err)) {
		return false;
	}
	*out = d != nullptr ? &d->params : nullptr;
	return true;
}

static bool single_pair(const dirvec *dirs, const char **a, const char **b, struct err **err) {
	const struct scfg_directive *d = nullptr;
	if (!single(dirs, &d, err)) {
		return false;
	}
	*a = *b = nullptr;
	if (d != nullptr) {
		if (d->params.len != 2) {
			return dir_err(err, d, "directive requires exactly 2 parameters");
		}
		*a = d->params.data[0];
		*b = d->params.data[1];
	}
	return true;
}

static bool parse_bool(const char *s, bool *out, struct err **err) {
	if (!parse_go_bool(str_from(s), out)) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(s));
		return err_set(err, 0, "strconv.ParseBool: parsing %s: invalid syntax", q);
	}
	return true;
}

// Borrowed driver and source ("" if missing)
static bool parse_driver_source(const char *name, const strvec *params, const char **driver,
				const char **source, struct err **err) {
	switch (params->len) {
	case 1:
		*driver = params->data[0];
		*source = "";
		return true;
	case 2:
		*driver = params->data[0];
		*source = params->data[1];
		return true;
	default:
		return err_set(err, 0, "directive %s requires exactly 1 or 2 parameters", name);
	}
}

bool parse_cidr(struct str s, struct ipnet *out, struct err **err) {
	if (!ipnet_parse(s, out)) {
		return err_set(err, 0, "invalid CIDR address: %.*s", STR_FMT(s));
	}
	*out = ipnet_trunc(*out);
	return true;
}

static void replace(char **field, const char *value) {
	char *v = xstrdup(value);
	xfree(*field);
	*field = v;
}

// Wraps the error of a nested call: "<prefix>: <message>"
static bool wrapped(struct err **err, struct err *e, const char *prefix) {
	err_set(err, 0, "%s: %s", prefix, err_msg(e));
	err_free(e);
	return false;
}

static bool parse_into(const struct scfg_block *block, struct config *srv, struct err **err) {
	dirvec dirs = {0};
	bool ok = false;
	struct err *e = nullptr;
	const char *s, *a, *b;
	const strvec *list;

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

	for (size_t i = 0; i < block->v.len; i++) {
		const struct scfg_directive *d = &block->v.data[i];
		bool known = false;
		for (size_t k = 0; k < N_KNOWN; k++) {
			known = known || cstr_eq(d->name, KNOWN_DIRECTIVES[k]);
		}
		if (!known) {
			dir_err(err, d, "unknown directive");
			goto out;
		}
	}

	get(block, "listen", &dirs);
	for (size_t i = 0; i < dirs.len; i++) {
		const struct scfg_directive *d = dirs.data[i];
		if (d->has_children) {
			dir_err(err, d, "directive requires zero children");
			goto out;
		}
		if (d->params.len != 1) {
			dir_err(err, d, "directive requires exactly one parameter");
			goto out;
		}
		strvec_push(&srv->listen, d->params.data[0]);
	}
	get(block, "hostname", &dirs);
	TRY(single_string(&dirs, &s, err));
	if (s != nullptr && s[0] != '\0') {
		replace(&srv->basic.hostname, s);
	}
	get(block, "title", &dirs);
	TRY(single_string(&dirs, &s, err));
	replace(&srv->basic.title, s != nullptr ? s : "");
	get(block, "icon", &dirs);
	TRY(single_string(&dirs, &s, err));
	replace(&srv->icon, s != nullptr ? s : "");
	get(block, "motd", &dirs);
	TRY(single_string(&dirs, &s, err));
	replace(&srv->motd_path, s != nullptr ? s : "");
	get(block, "tls", &dirs);
	TRY(single_pair(&dirs, &a, &b, err));
	if (a != nullptr) {
		srv->has_tls = true;
		srv->tls_cert_path = xstrdup(a);
		srv->tls_key_path = xstrdup(b);
	}
	get(block, "db", &dirs);
	TRY(single_pair(&dirs, &a, &b, err));
	if (a != nullptr) {
		if (cstr_eq(a, "postgres")) {
			FAIL("directive db: driver \"postgres\" is not supported");
		} else if (!cstr_eq(a, "sqlite3")) {
			AUTOFREE char *q = utf8_debug_cstr(str_from(a));
			FAIL("directive db: unknown driver %s", q);
		}
		replace(&srv->db_driver, a);
		replace(&srv->db_source, b);
	}

	get(block, "message-store", &dirs);
	TRY(single_list(&dirs, &list, err));
	if (list == nullptr) {
		get(block, "log", &dirs);
		TRY(single_list(&dirs, &list, err));
	}
	if (list != nullptr) {
		TRY(parse_driver_source("message-store", list, &a, &b, err));
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
		replace(&srv->basic.msg_store_source, b);
	}

	if (srv->basic.msg_store_driver == MSG_STORE_TURSO) {
		get(block, "db", &dirs);
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
			get(block, names_in_order[k], &dirs);
			TRY(single_string(&dirs, &vals[k], err));
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
				turso_free(&t);
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
			get(block, KNOWN_DIRECTIVES[k], &dirs);
			if (dirs.len > 0) {
				FAIL("turso directives require message-store turso");
			}
		}
	}

	get(block, "auth", &dirs);
	if (dirs.len > 0) {
		// drop default auth if we are explicitly defining any auth
		vec_free_with(&srv->auth, free_auth);
	}
	for (size_t i = 0; i < dirs.len; i++) {
		const struct scfg_directive *d = dirs.data[i];
		if (d->has_children) {
			dir_err(err, d, "directive requires zero children");
			goto out;
		}
		TRY(parse_driver_source("auth", &d->params, &a, &b, err));
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

	get(block, "file-upload", &dirs);
	TRY(single_list(&dirs, &list, err));
	if (list != nullptr) {
		TRY(parse_driver_source("file-upload", list, &a, &b, err));
		srv->file_upload = true;
	}

	get(block, "http-origin", &dirs);
	TRY(single_list(&dirs, &list, err));
	if (list != nullptr) {
		for (size_t i = 0; i < list->len; i++) {
			bool m;
			if (!path_match(str_from(list->data[i]), str_from(list->data[i]), &m)) {
				FAIL("directive http-origin: syntax error in pattern");
			}
		}
		strvec_copy(&srv->basic.http_origins, list);
	}

	get(block, "http-ingress", &dirs);
	TRY(single_string(&dirs, &s, err));
	if (s != nullptr && s[0] != '\0') {
		replace(&srv->basic.http_ingress, s);
	} else {
		char *v = xasprintf("https://%s", srv->basic.hostname);
		xfree(srv->basic.http_ingress);
		srv->basic.http_ingress = v;
	}

	get(block, "accept-proxy-ip", &dirs);
	TRY(single_list(&dirs, &list, err));
	for (size_t i = 0; list != nullptr && i < list->len; i++) {
		if (cstr_eq(list->data[i], "localhost")) {
			push_loopback(&srv->basic.accept_proxy_ips);
			continue;
		}
		struct ipnet n;
		if (!parse_cidr(str_from(list->data[i]), &n, &e)) {
			wrapped(err, e, "directive accept-proxy-ip: failed to parse CIDR");
			goto out;
		}
		vec_push(&srv->basic.accept_proxy_ips.v, n);
	}
	get(block, "accept-proxy-unix", &dirs);
	TRY(single_string(&dirs, &s, err));
	if (s != nullptr && s[0] != '\0') {
		if (!parse_bool(s, &srv->basic.accept_proxy_unix, &e)) {
			wrapped(err, e, "directive accept-proxy-unix");
			goto out;
		}
	}
	get(block, "max-user-networks", &dirs);
	TRY(single_string(&dirs, &s, err));
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
	get(block, "upstream-user-ip", &dirs);
	TRY(single_list(&dirs, &list, err));
	bool has_v4 = false, has_v6 = false;
	for (size_t i = 0; list != nullptr && i < list->len; i++) {
		struct ipnet n;
		if (!parse_cidr(str_from(list->data[i]), &n, &e)) {
			wrapped(err, e, "directive upstream-user-ip: failed to parse CIDR");
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
	get(block, "disable-inactive-user", &dirs);
	TRY(single_string(&dirs, &s, err));
	if (s != nullptr && s[0] != '\0') {
		if (!config_parse_duration(str_from(s), &srv->basic.disable_inactive_users_delay,
					   &e)) {
			wrapped(err, e, "directive disable-inactive-user");
			goto out;
		}
	}
	get(block, "message-expiry", &dirs);
	TRY(single_string(&dirs, &s, err));
	if (s != nullptr && s[0] != '\0') {
		if (!config_parse_duration(str_from(s), &srv->basic.message_expiry, &e)) {
			wrapped(err, e, "directive message-expiry");
			goto out;
		}
	}
	get(block, "chathistory-limit", &dirs);
	TRY(single_string(&dirs, &s, err));
	if (s != nullptr) {
		unsigned long long n;
		if (!str_parse_u64(str_from(s), &n) || n < MIN_CHAT_HISTORY_LIMIT ||
		    n > MAX_CHAT_HISTORY_LIMIT) {
			FAIL("directive chathistory-limit: must be an integer between %zu and %zu",
			     MIN_CHAT_HISTORY_LIMIT, MAX_CHAT_HISTORY_LIMIT);
		}
		srv->basic.chat_history_limit = (size_t)n;
	}
	get(block, "enable-user-on-auth", &dirs);
	TRY(single_string(&dirs, &s, err));
	if (s != nullptr && s[0] != '\0') {
		if (!parse_bool(s, &srv->basic.enable_users_on_auth, &e)) {
			wrapped(err, e, "directive enable-user-on-auth");
			goto out;
		}
	}
	get(block, "client-cert-auth", &dirs);
	TRY(single_string(&dirs, &s, err));
	if (s != nullptr && s[0] != '\0') {
		if (!parse_bool(s, &srv->basic.client_cert_auth, &e)) {
			wrapped(err, e, "directive client-cert-auth");
			goto out;
		}
	}
	ok = true;
out:
	vec_free(&dirs);
	return ok;
#undef TRY
#undef FAIL
}

bool config_parse(struct str s, struct config *out, struct err **err) {
	struct scfg_block block;
	if (!scfg_parse(s, &block, err)) {
		return false;
	}
	struct config c;
	config_defaults(&c);
	bool ok = parse_into(&block, &c, err);
	scfg_block_free(&block);
	if (!ok) {
		config_free(&c);
		return false;
	}
	*out = c;
	return true;
}

bool config_load(const char *filename, struct config *out, struct err **err) {
	int fd = open(filename, O_RDONLY | O_CLOEXEC);
	if (fd < 0) {
		return err_set(err, errno, "%s (os error %d)", strerror(errno), errno);
	}
	AUTO_STRBUF struct strbuf buf = STRBUF_INIT;
	char chunk[65536];
	while (true) {
		ssize_t n = read(fd, chunk, sizeof(chunk));
		if (n < 0) {
			if (errno == EINTR) {
				continue;
			}
			int e = errno;
			close(fd);
			return err_set(err, e, "%s (os error %d)", strerror(e), e);
		}
		if (n == 0) {
			break;
		}
		strbuf_append(&buf, chunk, (size_t)n);
	}
	close(fd);
	// read_to_string fails on invalid UTF-8
	if (!utf8_valid(strbuf_str(&buf))) {
		return err_set(err, 0, "stream did not contain valid UTF-8");
	}
	return config_parse(strbuf_str(&buf), out, err);
}
