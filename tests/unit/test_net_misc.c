// Ported from soju-rs src/proxyproto.rs, src/identd.rs, src/tls.rs and
// src/certfp.rs tests, plus C-only tests of TLS connections and listeners
#include "testsupport.h"

#include <arpa/inet.h>
#include <openssl/evp.h>
#include <openssl/x509.h>
#include <sys/socket.h>
#include <unistd.h>

#include "base/alloc.h"
#include "net/certfp.h"
#include "net/conn.h"
#include "net/identd.h"
#include "net/listener.h"
#include "net/proxyproto.h"
#include "net/tls.h"

static struct log_root root;

#define RUN_UNTIL(loop, cond, ms)                                            \
	do {                                                                 \
		uint64_t deadline_ = uv_hrtime() + (uint64_t)(ms) * 1000000; \
		while (!(cond) && uv_hrtime() < deadline_) {                 \
			uv_run((loop), UV_RUN_NOWAIT);                       \
		}                                                            \
	} while (0)

// proxyproto.rs

static enum proxy_result parse_all(struct str in, struct proxy_header *h, struct str *rest) {
	size_t consumed = 0;
	enum proxy_result r = proxy_parse(in, true, h, &consumed, nullptr);
	*rest = r == PROXY_HEADER ? str_slice(in, consumed, in.len) : in;
	return r;
}

static void test_no_header(void **state) {
	(void)state;
	struct proxy_header h = {0};
	struct str rest;
	assert_int_equal(parse_all(STR_LIT("PASS x\r\nNICK a\r\n"), &h, &rest), PROXY_NONE);
	assert_str_eq(rest, "PASS x\r\nNICK a\r\n");
	assert_int_equal(parse_all(STR_LIT("\r\nNICK a\r\n"), &h, &rest), PROXY_NONE);
	assert_str_eq(rest, "\r\nNICK a\r\n");
	// C-only: a prefix of a signature waits for more bytes
	size_t consumed;
	assert_int_equal(proxy_parse(STR_LIT("PROX"), false, &h, &consumed, nullptr),
			 PROXY_NEED_MORE);
}

static void test_v1(void **state) {
	(void)state;
	struct proxy_header h = {0};
	struct str rest;
	assert_int_equal(
		parse_all(STR_LIT("PROXY TCP4 1.2.3.4 5.6.7.8 1111 6667\r\nNICK a\r\n"), &h, &rest),
		PROXY_HEADER);
	assert_true(h.has_source);
	struct ipaddr want;
	assert_true(ipaddr_parse(STR_LIT("1.2.3.4"), &want));
	assert_memory_equal(h.source_ip.b, want.b, 4);
	assert_int_equal(h.source_port, 1111);
	assert_true(ipaddr_parse(STR_LIT("5.6.7.8"), &want));
	assert_memory_equal(h.destination_ip.b, want.b, 4);
	assert_int_equal(h.destination_port, 6667);
	assert_str_eq(rest, "NICK a\r\n");
	proxy_header_free(&h);
	assert_int_equal(parse_all(STR_LIT("PROXY UNKNOWN\r\n"), &h, &rest), PROXY_HEADER);
	assert_false(h.has_source);
	proxy_header_free(&h);
}

static void test_v2(void **state) {
	(void)state;
	static const char cert[] = "fake-der";
	struct strbuf ssl = STRBUF_INIT;
	strbuf_append(&ssl, "\x01\0\0\0\0", 5);
	strbuf_append_char(&ssl, 0x28);
	strbuf_append_char(&ssl, 0);
	strbuf_append_char(&ssl, (char)(sizeof(cert) - 1));
	strbuf_append(&ssl, cert, sizeof(cert) - 1);
	struct strbuf body = STRBUF_INIT;
	strbuf_append(&body, "\x0a\0\0\x01\x0a\0\0\x02\x04\xd2\x1a\x0b", 12);
	strbuf_append_char(&body, 0x20);
	strbuf_append_char(&body, 0);
	strbuf_append_char(&body, (char)ssl.len);
	strbuf_append(&body, ssl.buf, ssl.len);
	struct strbuf in = STRBUF_INIT;
	strbuf_append(&in, "\r\n\r\n\0\r\nQUIT\n", 12);
	strbuf_append(&in, "\x21\x11", 2);
	strbuf_append_char(&in, 0);
	strbuf_append_char(&in, (char)body.len);
	strbuf_append(&in, body.buf, body.len);
	strbuf_append_cstr(&in, "NICK a\r\n");
	struct proxy_header h = {0};
	struct str rest;
	assert_int_equal(parse_all(strbuf_str(&in), &h, &rest), PROXY_HEADER);
	assert_true(h.has_source);
	assert_memory_equal(h.source_ip.b, "\x0a\0\0\x01", 4);
	assert_int_equal(h.source_port, 1234);
	assert_memory_equal(h.destination_ip.b, "\x0a\0\0\x02", 4);
	assert_int_equal(h.destination_port, 6667);
	assert_true(h.has_client_cert);
	assert_int_equal(h.client_cert.len, sizeof(cert) - 1);
	assert_memory_equal(h.client_cert.buf, cert, sizeof(cert) - 1);
	assert_str_eq(rest, "NICK a\r\n");
	proxy_header_free(&h);
	strbuf_free(&in);
	strbuf_free(&body);
	strbuf_free(&ssl);
}

// identd.rs

static void test_query(void **state) {
	(void)state;
	struct identd id;
	identd_init(&id);
	identd_store(&id, "10.0.0.1:6667", "10.0.0.2:40000", "abcdef");
	uv_loop_t loop;
	uv_loop_init(&loop);
	int fds[2];
	assert_int_equal(socketpair(AF_UNIX, SOCK_STREAM, 0, fds), 0);
	uv_pipe_t *p = xcalloc(1, sizeof(*p));
	uv_pipe_init(&loop, p, 0);
	uv_pipe_open(p, fds[0]);
	identd_serve(&id, (uv_stream_t *)p, "10.0.0.1", false);
	assert_true(write(fds[1], "40000 , 6667\r\n", 14) == 14);
	char buf[256] = {0};
	size_t got = 0;
	const char want1[] = "40000 , 6667 : USERID : UNKNOWN : abcdef\r\n";
	RUN_UNTIL(&loop,
		  (got += (size_t)(recv(fds[1], buf + got, sizeof(buf) - 1 - got, MSG_DONTWAIT) > 0
					   ? strlen(buf + got)
					   : 0),
		   got >= strlen(want1)),
		  2000);
	assert_string_equal(buf, want1);
	assert_true(write(fds[1], "1, 2\r\n", 6) == 6);
	memset(buf, 0, sizeof(buf));
	got = 0;
	const char want2[] = "1, 2 : ERROR : NO-USER\r\n";
	RUN_UNTIL(&loop,
		  (got += (size_t)(recv(fds[1], buf + got, sizeof(buf) - 1 - got, MSG_DONTWAIT) > 0
					   ? strlen(buf + got)
					   : 0),
		   got >= strlen(want2)),
		  2000);
	assert_string_equal(buf, want2);
	uv_run(&loop, UV_RUN_DEFAULT);
	assert_int_equal(uv_loop_close(&loop), 0);
	close(fds[1]);
	uint16_t a, b;
	assert_false(identd_parse_query("0,1", &a, &b));
	assert_false(identd_parse_query("x", &a, &b));
	identd_finish(&id);
}

// certfp.rs

static void test_generate_all(void **state) {
	(void)state;
	const char *types[] = {"ecdsa", "ed25519", "rsa"};
	const unsigned bits[] = {0, 0, 2048};
	for (size_t i = 0; i < 3; i++) {
		struct strbuf key = STRBUF_INIT, cert = STRBUF_INIT;
		struct err *e = nullptr;
		if (!generate_certfp(types[i], bits[i], &key, &cert, &e)) {
			fail_msg("%s: %s", types[i], err_msg(e));
		}
		assert_true(key.len > 0 && cert.len > 0);
		// The pair must be usable as a TLS client certificate
		SSL_CTX *ctx = tls_client_ctx("", strbuf_str(&cert), strbuf_str(&key), &e);
		if (ctx == nullptr) {
			fail_msg("%s: %s", types[i], err_msg(e));
		}
		SSL_CTX_free(ctx);
		strbuf_free(&key);
		strbuf_free(&cert);
	}
	struct strbuf key = STRBUF_INIT, cert = STRBUF_INIT;
	struct err *e = nullptr;
	assert_false(generate_certfp("rsa", 1000, &key, &cert, &e));
	err_free(e);
	e = nullptr;
	assert_false(generate_certfp("dsa", 0, &key, &cert, &e));
	assert_string_equal(err_msg(e), "x509: unknown key type while marshaling PKCS#8: <nil>");
	err_free(e);
}

// tls.rs

struct pair {
	X509 *cert;
	EVP_PKEY *key;
	struct strbuf der;
};

static void self_signed(struct pair *p) {
	struct strbuf key = STRBUF_INIT;
	p->der = (struct strbuf){0};
	assert_true(generate_certfp("ed25519", 0, &key, &p->der, nullptr));
	const unsigned char *k = (const unsigned char *)key.buf;
	p->key = d2i_AutoPrivateKey(nullptr, &k, (long)key.len);
	const unsigned char *c = (const unsigned char *)p->der.buf;
	p->cert = d2i_X509(nullptr, &c, (long)p->der.len);
	assert_non_null(p->key);
	assert_non_null(p->cert);
	strbuf_free(&key);
}

static void pair_free(struct pair *p) {
	X509_free(p->cert);
	EVP_PKEY_free(p->key);
	strbuf_free(&p->der);
}

struct tls_peer {
	struct conn *c;
	bool ready, closed;
	char *err;
	irc_msg_vec got;
};

static void tp_ready(struct conn *c, void *data) {
	(void)c;
	((struct tls_peer *)data)->ready = true;
}

static void tp_message(struct conn *c, struct irc_msg *m, void *data) {
	vec_push(&((struct tls_peer *)data)->got, m);
	conn_input_done(c);
}

static void tp_closed(struct conn *c, const struct err *err, void *data) {
	(void)c;
	struct tls_peer *p = data;
	p->closed = true;
	if (err != nullptr && p->err == nullptr) {
		p->err = xstrdup(err_msg(err));
	}
}

static const struct conn_callbacks tp_cb = {
	.ready = tp_ready, .message = tp_message, .closed = tp_closed};

// A handshake against a server presenting chain (signed with key) while
// the client pins `pinned`; returns whether a message got through
static bool handshake(X509 **chain, size_t n, EVP_PKEY *key, const struct strbuf *pinned,
		      int version, char **client_err) {
	struct logger log;
	log_init(&log, &root);
	struct tls_cert_store store;
	tls_cert_store_init(&store);
	store.chain = sk_X509_new_null();
	for (size_t i = 0; i < n; i++) {
		X509_up_ref(chain[i]);
		sk_X509_push(store.chain, chain[i]);
	}
	EVP_PKEY_up_ref(key);
	store.key = key;
	SSL_CTX *sctx = tls_server_ctx(&store, "irc", nullptr, nullptr);
	SSL_CTX_set_min_proto_version(sctx, version);
	SSL_CTX_set_max_proto_version(sctx, version);
	unsigned char md[32];
	unsigned mdlen;
	EVP_Digest(pinned->buf, pinned->len, md, &mdlen, EVP_sha256(), nullptr);
	struct strbuf fp = STRBUF_INIT;
	strbuf_append_cstr(&fp, "sha-256:");
	for (unsigned i = 0; i < mdlen; i++) {
		strbuf_printf(&fp, "%02x", md[i]);
	}
	SSL_CTX *cctx = tls_client_ctx(strbuf_cstr(&fp), STR_EMPTY, STR_EMPTY, nullptr);
	strbuf_free(&fp);

	uv_loop_t loop;
	uv_loop_init(&loop);
	int fds[2];
	assert_int_equal(socketpair(AF_UNIX, SOCK_STREAM, 0, fds), 0);
	struct tls_peer s = {0}, c = {0};
	uv_pipe_t *ps = xcalloc(1, sizeof(*ps)), *pc = xcalloc(1, sizeof(*pc));
	uv_pipe_init(&loop, ps, 0);
	uv_pipe_open(ps, fds[0]);
	uv_pipe_init(&loop, pc, 0);
	uv_pipe_open(pc, fds[1]);
	struct conn_options so = {.tls_server = sctx, .setup_timeout_ms = 5000 * T_TIMEOUT_SCALE};
	struct conn_options co = {.tls_client = cctx,
				  .tls_host = "example.org",
				  .setup_timeout_ms = 5000 * T_TIMEOUT_SCALE};
	s.c = conn_new((uv_stream_t *)ps, &so, &log, &tp_cb, &s);
	c.c = conn_new((uv_stream_t *)pc, &co, &log, &tp_cb, &c);
	struct irc_msg *m = IRC_MSG("PING", "x");
	conn_send(s.c, m);
	irc_msg_free(m);
	RUN_UNTIL(&loop, c.got.len == 1 || c.closed || s.closed, 5000);
	bool ok = c.got.len == 1;
	assert_true(ok == c.ready);
	if (!ok && client_err != nullptr) {
		*client_err = c.err != nullptr ? xstrdup(c.err) : nullptr;
	}
	conn_close(s.c);
	conn_close(c.c);
	conn_unref(s.c);
	conn_unref(c.c);
	uv_run(&loop, UV_RUN_DEFAULT);
	assert_int_equal(uv_loop_close(&loop), 0);
	irc_msg_vec_free(&s.got);
	irc_msg_vec_free(&c.got);
	xfree(s.err);
	xfree(c.err);
	SSL_CTX_free(sctx);
	SSL_CTX_free(cctx);
	tls_cert_store_finish(&store);
	log_free(&log);
	return ok;
}

static void test_pinned_cert(void **state) {
	(void)state;
	const int versions[] = {TLS1_2_VERSION, TLS1_3_VERSION};
	for (size_t v = 0; v < 2; v++) {
		struct pair a, b;
		self_signed(&a);
		self_signed(&b);
		assert_true(handshake((X509 *[]){a.cert}, 1, a.key, &a.der, versions[v], nullptr));
		// An unrelated leaf followed by the pinned certificate must be
		// rejected
		char *err = nullptr;
		assert_false(
			handshake((X509 *[]){b.cert, a.cert}, 2, b.key, &a.der, versions[v], &err));
		assert_non_null(err);
		assert_non_null(strstr(err, "fingerprint doesn't match the server's"));
		xfree(err);
		pair_free(&a);
		pair_free(&b);
	}
}

// C-only: a listener trusting PROXY headers
struct listener_test {
	struct conn *c;
	char *remote_addr;
	irc_msg_vec got;
};

static void lt_message(struct conn *c, struct irc_msg *m, void *data) {
	vec_push(&((struct listener_test *)data)->got, m);
	conn_input_done(c);
}

static void lt_irc(struct conn *c, const struct conn_info *info, void *data) {
	struct listener_test *t = data;
	t->c = c;
	t->remote_addr = xstrdup(info->remote_addr);
	conn_set_callbacks(c, &(struct conn_callbacks){.message = lt_message}, t);
}

static bool trust_all(const struct ipaddr *ip, void *data) {
	(void)ip;
	(void)data;
	return true;
}

static void on_connect(uv_connect_t *req, int status) {
	assert_int_equal(status, 0);
	static const char hello[] = "PROXY TCP4 1.2.3.4 5.6.7.8 1111 6667\r\nNICK a\r\n";
	uv_buf_t buf = uv_buf_init((char *)(uintptr_t)hello, sizeof(hello) - 1);
	assert_true(uv_try_write(req->handle, &buf, 1) == (int)(sizeof(hello) - 1));
}

static void test_listener_proxy(void **state) {
	(void)state;
	struct logger log;
	log_init(&log, &root);
	struct loop_pool pool;
	loop_pool_init(&pool, 1);
	loop_set_current(&pool.loops[0]);
	uv_loop_t *loop = &pool.loops[0].uv;
	struct listener_test t = {0};
	struct listener_handlers h = {.irc = lt_irc, .accept_proxy = trust_all, .data = &t};
	struct listener_spec spec;
	assert_true(parse_listen_uri("irc+insecure://127.0.0.1:0", &spec, nullptr));
	struct listener *l;
	uint16_t port;
	struct err *e = nullptr;
	if (!listener_start(&pool, &spec, nullptr, &h, &log, &l, &port, &e)) {
		fail_msg("%s", err_msg(e));
	}
	assert_true(port != 0);
	uv_tcp_t client;
	uv_tcp_init(loop, &client);
	struct sockaddr_in addr;
	uv_ip4_addr("127.0.0.1", port, &addr);
	uv_connect_t req;
	assert_int_equal(uv_tcp_connect(&req, &client, (struct sockaddr *)&addr, on_connect), 0);
	RUN_UNTIL(loop, t.got.len == 1, 3000);
	assert_int_equal(t.got.len, 1);
	assert_string_equal(t.remote_addr, "1.2.3.4:1111");
	assert_msg_str(t.got.data[0], "NICK a");
	conn_close(t.c);
	conn_unref(t.c);
	uv_close((uv_handle_t *)&client, nullptr);
	listener_stop(l);
	loop_close_inbox(&pool.loops[0]);
	uv_run(loop, UV_RUN_DEFAULT);
	loop_set_current(nullptr);
	loop_pool_free(&pool);
	irc_msg_vec_free(&t.got);
	xfree(t.remote_addr);
	listener_spec_free(&spec);
	log_free(&log);
}

T_MAIN(cmocka_unit_test(test_no_header), cmocka_unit_test(test_v1), cmocka_unit_test(test_v2),
       cmocka_unit_test(test_query), cmocka_unit_test(test_generate_all),
       cmocka_unit_test(test_pinned_cert), cmocka_unit_test(test_listener_proxy))
