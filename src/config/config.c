#include "config/internal.h"

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

void config_free_auth(struct config_auth *a) {
	xfree(a->source);
}

void config_turso_free(struct config_turso *t) {
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
	vec_free_with(&c->auth, config_free_auth);
	config_turso_free(&c->turso);
	*c = (struct config){0};
}

bool config_parse(struct str s, struct config *out, struct err **err) {
	struct scfg_block block;
	if (!scfg_parse(s, &block, err)) {
		return false;
	}
	struct config c;
	config_defaults(&c);
	bool ok = config_parse_block(&block, &c, err);
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
