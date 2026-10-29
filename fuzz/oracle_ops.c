#include "oracle_ops.h"

#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

#include "base/alloc.h"
#include "base/base64.h"
#include "base/path_match.h"
#include "base/timeutil.h"
#include "base/utf8.h"
#include "config/config.h"
#include "config/scfg.h"
#include "db/utils.h"
#include "irc/casemapping.h"
#include "irc/ircutil.h"
#include "irc/message.h"
#include "irc/multiline.h"
#include "irc/whox.h"
#include "msgstore/msgid.h"
#include "net/ipaddr.h"
#include "net/listen_uri.h"
#include "net/url.h"

// In the order fuzz inputs select them: keep appending
const char *const oracle_ops[] = {
	"irc",    "time",    "lossy",     "lower",       "upper",  "debug",    "scfg",
	"config", "casemap", "pathmatch", "split_words", "url",    "hostport", "highlight",
	"listen", "msgid",   "strip",     "b64std",      "b64url", "ipaddr",   "sockaddr",
	"ipnet",  "mlimits", "whox",      nullptr,
};

static void dbg(struct strbuf *out, const char *s) {
	utf8_debug_str(out, str_from(s));
}

// format!("{:?}", Vec<String>)
static void dbg_vec(struct strbuf *out, const char *const *v, size_t n) {
	strbuf_append_char(out, '[');
	for (size_t i = 0; i < n; i++) {
		if (i > 0) {
			strbuf_append_cstr(out, ", ");
		}
		dbg(out, v[i]);
	}
	strbuf_append_char(out, ']');
}

static void split_two(struct str s, struct str *a, struct str *b) {
	if (!str_split_once(s, '\0', a, b)) {
		*a = s;
		*b = STR_EMPTY;
	}
}

static void scfg_dump(const struct scfg_block *b, struct strbuf *out, size_t depth) {
	for (size_t i = 0; i < b->v.len; i++) {
		const struct scfg_directive *d = &b->v.data[i];
		strbuf_printf(out, "%*s%s ", (int)depth, "", d->name);
		dbg_vec(out, (const char *const *)d->params.data, d->params.len);
		strbuf_printf(out, " @%zu", d->lineno);
		if (d->has_children) {
			strbuf_append_cstr(out, " {\n");
			scfg_dump(&d->children, out, depth + 1);
			strbuf_printf(out, "%*s}\n", (int)depth, "");
		} else {
			strbuf_append_char(out, '\n');
		}
	}
}

static void ipnet_str(struct strbuf *out, struct ipnet n) {
	ipaddr_format(out, n.addr);
	strbuf_printf(out, "/%u", n.prefix);
}

static void dbg_ipnets(struct strbuf *out, const struct ipnet *v, size_t n) {
	strbuf_append_char(out, '[');
	for (size_t i = 0; i < n; i++) {
		if (i > 0) {
			strbuf_append_cstr(out, ", ");
		}
		struct strbuf t = STRBUF_INIT;
		ipnet_str(&t, v[i]);
		dbg(out, strbuf_cstr(&t));
		strbuf_free(&t);
	}
	strbuf_append_char(out, ']');
}

static const char *bstr(bool b) {
	return b ? "true" : "false";
}

static void config_dump(const struct config *c, struct strbuf *out) {
	const struct config_basic *b = &c->basic;
	static const char *const stores[] = {"Memory", "Db", "Turso"};
	static const char *const auths[] = {"Internal", "Pam", "Http", "OAuth2"};
	strbuf_append_cstr(out, "ok:\nlisten ");
	dbg_vec(out, (const char *const *)c->listen.data, c->listen.len);
	strbuf_append_cstr(out, "\ntitle ");
	dbg(out, b->title);
	strbuf_printf(out, "\nstore %s ", stores[b->msg_store_driver]);
	dbg(out, b->msg_store_source);
	strbuf_append_cstr(out, "\norigins ");
	dbg_vec(out, (const char *const *)b->http_origins.data, b->http_origins.len);
	strbuf_append_cstr(out, "\nproxy ");
	dbg_ipnets(out, b->accept_proxy_ips.v.data, b->accept_proxy_ips.v.len);
	strbuf_printf(out, " %s\nmaxnet %lld\nupips ", bstr(b->accept_proxy_unix),
		      (long long)b->max_user_networks);
	dbg_ipnets(out, b->upstream_user_ips.data, b->upstream_user_ips.len);
	strbuf_printf(out, "\ndurations %llu %llu\n",
		      (unsigned long long)b->disable_inactive_users_delay,
		      (unsigned long long)b->message_expiry);
	strbuf_printf(out, "limit %zu %s %s\ntls ", b->chat_history_limit,
		      bstr(b->enable_users_on_auth), bstr(b->client_cert_auth));
	if (c->has_tls) {
		strbuf_append_cstr(out, "Some((");
		dbg(out, c->tls_cert_path);
		strbuf_append_cstr(out, ", ");
		dbg(out, c->tls_key_path);
		strbuf_append_cstr(out, "))");
	} else {
		strbuf_append_cstr(out, "None");
	}
	strbuf_append_cstr(out, "\nmisc ");
	dbg(out, c->icon);
	strbuf_append_char(out, ' ');
	dbg(out, c->motd_path);
	strbuf_append_char(out, ' ');
	dbg(out, c->db_driver);
	strbuf_append_char(out, ' ');
	dbg(out, c->db_source);
	strbuf_printf(out, " %s\nauth [", bstr(c->file_upload));
	for (size_t i = 0; i < c->auth.len; i++) {
		if (i > 0) {
			strbuf_append_cstr(out, ", ");
		}
		strbuf_append_cstr(out, "(");
		dbg(out, auths[c->auth.data[i].driver]);
		strbuf_append_cstr(out, ", ");
		dbg(out, c->auth.data[i].source);
		strbuf_append_cstr(out, ")");
	}
	strbuf_append_cstr(out, "]\n");
	if (c->has_turso) {
		const struct config_turso *t = &c->turso;
		strbuf_append_cstr(out, "turso ");
		const char *f[] = {t->organization, t->group, t->controller, t->group_token,
				   t->api_token};
		for (size_t i = 0; i < 5; i++) {
			if (i > 0) {
				strbuf_append_char(out, ' ');
			}
			dbg(out, f[i]);
		}
		strbuf_append_char(out, '\n');
	}
}

static void hex(struct strbuf *out, const struct strbuf *b) {
	for (size_t i = 0; i < b->len; i++) {
		strbuf_printf(out, "%02x", (unsigned char)b->buf[i]);
	}
}

static void err_out(struct strbuf *out, struct err *e) {
	strbuf_printf(out, "err:%s", err_msg(e));
	err_free(e);
}

void oracle_run_c(const char *op, struct str raw, struct strbuf *out) {
	struct strbuf lossy_b = STRBUF_INIT;
	utf8_lossy(&lossy_b, raw);
	// C strings stop at NUL: ops on NUL-free text only, see oracle_input_ok
	struct str s = strbuf_str(&lossy_b);
	const char *cs = strbuf_cstr(&lossy_b);
	struct err *e = nullptr;
	struct str a, b;

	if (cstr_eq(op, "irc")) {
		enum irc_parse_error pe;
		struct irc_msg *m = irc_msg_parse(s, &pe);
		if (m != nullptr) {
			strbuf_append_cstr(out, "ok:");
			irc_msg_format(m, out);
			irc_msg_free(m);
		} else {
			strbuf_printf(out, "err:%s", irc_parse_error_str(pe));
		}
	} else if (cstr_eq(op, "time")) {
		struct timestamp t;
		if (parse_server_time(s, &t)) {
			strbuf_append_cstr(out, "some:");
			format_server_time(out, t);
			strbuf_printf(out, ":%lld.%u", (long long)t.sec, t.nsec);
		} else {
			strbuf_append_cstr(out, "none");
		}
	} else if (cstr_eq(op, "lossy")) {
		strbuf_append_str(out, s);
	} else if (cstr_eq(op, "lower")) {
		utf8_to_lower(out, s);
	} else if (cstr_eq(op, "upper")) {
		utf8_to_upper(out, s);
	} else if (cstr_eq(op, "debug")) {
		utf8_debug_str(out, s);
	} else if (cstr_eq(op, "scfg")) {
		struct scfg_block blk;
		if (scfg_parse(s, &blk, &e)) {
			strbuf_append_cstr(out, "ok:\n");
			scfg_dump(&blk, out, 0);
			scfg_block_free(&blk);
		} else {
			err_out(out, e);
		}
	} else if (cstr_eq(op, "config")) {
		struct config c;
		if (config_parse(s, &c, &e)) {
			config_dump(&c, out);
			config_free(&c);
		} else {
			err_out(out, e);
		}
	} else if (cstr_eq(op, "casemap")) {
		static const enum casemapping cms[] = {CASEMAPPING_ASCII, CASEMAPPING_RFC1459,
						       CASEMAPPING_RFC1459_STRICT};
		for (size_t i = 0; i < 3; i++) {
			char *m = casemap(cms[i], s);
			strbuf_printf(out, i > 0 ? "\n%s" : "%s", m);
			xfree(m);
		}
	} else if (cstr_eq(op, "pathmatch")) {
		split_two(s, &a, &b);
		bool m;
		strbuf_append_cstr(out, path_match(a, b, &m) ? bstr(m) : "err");
	} else if (cstr_eq(op, "split_words")) {
		strvec w = {0};
		if (split_words(s, &w, &e)) {
			strbuf_append_cstr(out, "ok:");
			dbg_vec(out, (const char *const *)w.data, w.len);
			strvec_free(&w);
		} else {
			err_out(out, e);
		}
	} else if (cstr_eq(op, "url")) {
		struct url u;
		if (url_parse(s, &u, &e)) {
			strbuf_append_cstr(out, "ok:");
			dbg(out, u.scheme);
			strbuf_append_char(out, '|');
			dbg(out, u.host);
			strbuf_append_char(out, '|');
			dbg(out, u.path);
			strbuf_printf(out, "|%s|", bstr(u.has_user));
			dbg(out, u.query);
			strbuf_append_char(out, '|');
			dbg(out, u.fragment);
			url_free(&u);
		} else {
			err_out(out, e);
		}
	} else if (cstr_eq(op, "hostport")) {
		char *h, *p;
		if (split_host_port(s, &h, &p, &e)) {
			strbuf_append_cstr(out, "ok:");
			dbg(out, h);
			strbuf_append_char(out, ' ');
			dbg(out, p);
			xfree(h);
			xfree(p);
		} else {
			err_out(out, e);
		}
	} else if (cstr_eq(op, "highlight")) {
		split_two(s, &a, &b);
		strbuf_append_cstr(out, b.len == 0 ? "skip" : bstr(is_highlight(a, b)));
	} else if (cstr_eq(op, "listen")) {
		static const char *const kinds[] = {"Irc", "Http", "Ident", "Prometheus", "Admin"};
		struct listener_spec l;
		if (parse_listen_uri(cs, &l, &e)) {
			strbuf_printf(out, "ok:%s %s(", kinds[l.kind], l.unix ? "Unix" : "Tcp");
			dbg(out, l.addr);
			strbuf_printf(out, ") %s", bstr(l.tls));
			listener_spec_free(&l);
		} else {
			err_out(out, e);
		}
	} else if (cstr_eq(op, "msgid")) {
		int64_t n;
		struct strbuf t = STRBUF_INIT;
		if (parse_msg_id(s, &n, &t, &e)) {
			strbuf_printf(out, "ok:%lld ", (long long)n);
			utf8_debug_str(out, strbuf_str(&t));
			strbuf_free(&t);
		} else {
			err_out(out, e);
		}
	} else if (cstr_eq(op, "strip")) {
		strip_ansi(out, s);
	} else if (cstr_eq(op, "b64std") || cstr_eq(op, "b64url")) {
		struct strbuf d = STRBUF_INIT;
		if (base64_decode(&d,
				  cstr_eq(op, "b64std") ? BASE64_STANDARD : BASE64_URL_SAFE_NO_PAD,
				  raw, &e)) {
			strbuf_append_cstr(out, "ok:");
			hex(out, &d);
		} else {
			err_out(out, e);
		}
		strbuf_free(&d);
	} else if (cstr_eq(op, "ipaddr")) {
		struct ipaddr ip;
		if (ipaddr_parse(s, &ip)) {
			strbuf_append_cstr(out, "ok:");
			ipaddr_format(out, ip);
			strbuf_append_char(out, ' ');
			ipaddr_format(out, ipaddr_canonical(ip));
		} else {
			strbuf_append_cstr(out, "err");
		}
	} else if (cstr_eq(op, "sockaddr")) {
		struct ipaddr ip;
		uint16_t port;
		if (socketaddr_parse(s, &ip, &port)) {
			strbuf_append_cstr(out, "ok:");
			ipaddr_format(out, ip);
			strbuf_printf(out, " %u", port);
		} else {
			strbuf_append_cstr(out, "err");
		}
	} else if (cstr_eq(op, "ipnet")) {
		struct ipnet n;
		if (ipnet_parse(s, &n)) {
			strbuf_append_cstr(out, "ok:");
			ipnet_str(out, ipnet_trunc(n));
		} else {
			strbuf_append_cstr(out, "err");
		}
	} else if (cstr_eq(op, "mlimits")) {
		char *v = multiline_limits_cap_value(multiline_limits_parse(s));
		strbuf_append_cstr(out, v);
		xfree(v);
	} else if (cstr_eq(op, "whox")) {
		char *f, *t;
		parse_whox_options(s, &f, &t);
		dbg(out, f);
		strbuf_append_char(out, ' ');
		dbg(out, t);
		xfree(f);
		xfree(t);
	} else {
		abort();
	}
	strbuf_free(&lossy_b);
}

struct oracle {
	pid_t pid;
	FILE *in, *out;
};

struct oracle *oracle_start(void) {
	const char *path = getenv("SOJU_ORACLE");
	if (path == nullptr) {
		path = SOJU_ORACLE_DEFAULT;
	}
	if (access(path, X_OK) != 0) {
		return nullptr;
	}
	int to[2], from[2];
	if (pipe(to) != 0 || pipe(from) != 0) {
		abort();
	}
	pid_t pid = fork();
	if (pid < 0) {
		abort();
	}
	if (pid == 0) {
		dup2(to[0], 0);
		dup2(from[1], 1);
		close(to[1]);
		close(from[0]);
		// SOJU_ORACLE is set by whoever runs the tests
		// NOLINTNEXTLINE(clang-analyzer-optin.taint.GenericTaint)
		execl(path, path, (char *)nullptr);
		_exit(127);
	}
	close(to[0]);
	close(from[1]);
	struct oracle *o = xmalloc(sizeof(*o));
	o->pid = pid;
	o->in = fdopen(to[1], "w");
	o->out = fdopen(from[0], "r");
	return o;
}

void oracle_stop(struct oracle *o) {
	if (o == nullptr) {
		return;
	}
	fclose(o->in);
	fclose(o->out);
	waitpid(o->pid, nullptr, 0);
	xfree(o);
}

static int unhex(int c) {
	return c >= 'a' ? c - 'a' + 10 : c - '0';
}

void oracle_run_rust(struct oracle *o, const char *op, struct str input, struct strbuf *out) {
	fprintf(o->in, "%s\t", op);
	for (size_t i = 0; i < input.len; i++) {
		fprintf(o->in, "%02x", (unsigned char)input.p[i]);
	}
	fputc('\n', o->in);
	fflush(o->in);
	int hi;
	while ((hi = fgetc(o->out)) != EOF && hi != '\n') {
		int lo = fgetc(o->out);
		if (lo == EOF) {
			abort();
		}
		strbuf_append_char(out, (char)(((unsigned)unhex(hi) << 4U) | (unsigned)unhex(lo)));
	}
	if (hi == EOF) {
		// The oracle died (a Rust panic)
		fprintf(stderr, "oracle died on op %s\n", op);
		abort();
	}
}
