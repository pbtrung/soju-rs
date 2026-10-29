// L3 lifecycle tests of connections (PLAN.md 2.3): the peer disconnects
// after every byte of a session, during the PROXY header and the TLS
// handshake. Every run must end with the connection closed, its callbacks
// called once, and nothing leaked (cmocka/ASan/valgrind check).
#include "testsupport.h"

#include <sys/socket.h>
#include <unistd.h>

#include "base/alloc.h"
#include "net/certfp.h"
#include "net/conn.h"
#include "net/tls.h"

static struct log_root root;

struct peer {
	size_t messages;
	unsigned closed;
	bool ready;
};

static void on_ready(struct conn *c, void *data) {
	(void)c;
	((struct peer *)data)->ready = true;
}

static void on_message(struct conn *c, struct irc_msg *m, void *data) {
	irc_msg_free(m);
	((struct peer *)data)->messages++;
	conn_input_done(c);
}

static void on_closed(struct conn *c, const struct err *err, void *data) {
	(void)c;
	(void)err;
	((struct peer *)data)->closed++;
}

static const struct conn_callbacks cb = {
	.ready = on_ready, .message = on_message, .closed = on_closed};

// Feeds the first k bytes of input to a connection, then closes the peer
static struct peer run(struct str input, size_t k, const struct conn_options *opts) {
	struct logger log;
	log_init(&log, &root);
	uv_loop_t loop;
	uv_loop_init(&loop);
	int fds[2];
	assert_int_equal(socketpair(AF_UNIX, SOCK_STREAM, 0, fds), 0);
	uv_pipe_t *p = xcalloc(1, sizeof(*p));
	uv_pipe_init(&loop, p, 0);
	uv_pipe_open(p, fds[0]);
	struct peer peer = {0};
	struct conn *c = conn_new((uv_stream_t *)p, opts, &log, &cb, &peer);
	if (k > 0) {
		assert_true(write(fds[1], input.p, k) == (ssize_t)k);
	}
	close(fds[1]);
	uint64_t deadline = uv_hrtime() + 5000000000ULL;
	while (peer.closed == 0 && uv_hrtime() < deadline) {
		uv_run(&loop, UV_RUN_NOWAIT);
	}
	assert_int_equal(peer.closed, 1);
	conn_unref(c);
	uv_run(&loop, UV_RUN_DEFAULT);
	assert_int_equal(uv_loop_close(&loop), 0);
	log_free(&log);
	return peer;
}

static void test_disconnect_at_every_byte(void **state) {
	(void)state;
	static const char session[] =
		"PROXY TCP4 1.2.3.4 5.6.7.8 1111 6667\r\nCAP LS 302\r\nNICK alice\r\n"
		"USER alice 0 * :Alice\r\nCAP REQ :sasl message-tags\r\nAUTHENTICATE PLAIN\r\n"
		"AUTHENTICATE AGFsaWNlAHB3\r\nCAP END\r\nJOIN #soju\r\n"
		"CHATHISTORY LATEST #soju * 100\r\nPRIVMSG BouncerServ :network status\r\n";
	struct str in = str_new(session, sizeof(session) - 1);
	struct conn_options opts = {.proxy = true, .setup_timeout_ms = 5000};
	size_t lines = 0;
	for (size_t k = 0; k <= in.len; k++) {
		struct peer p = run(in, k, &opts);
		// Only complete lines are delivered (the header isn't a message)
		size_t complete = 0;
		for (size_t i = 0; i < k; i++) {
			complete += session[i] == '\n';
		}
		assert_int_equal(p.messages, complete > 0 ? complete - 1 : 0);
		lines = p.messages;
	}
	assert_int_equal(lines, 10);
}

static void test_tls_truncated_handshake(void **state) {
	(void)state;
	// A real ClientHello
	struct strbuf key = STRBUF_INIT, cert = STRBUF_INIT;
	assert_true(generate_certfp("ed25519", 0, &key, &cert, nullptr));
	SSL_CTX *cctx = tls_client_ctx("sha-256:00", STR_EMPTY, STR_EMPTY, nullptr);
	struct tls_session t;
	assert_true(tls_session_init_client(&t, cctx, "example.org"));
	(void)tls_session_handshake(&t);
	struct strbuf hello = STRBUF_INIT;
	assert_true(tls_session_take_output(&t, &hello));
	tls_session_finish(&t);
	SSL_CTX_free(cctx);

	struct tls_cert_store store;
	tls_cert_store_init(&store);
	const unsigned char *kp = (const unsigned char *)key.buf;
	store.key = d2i_AutoPrivateKey(nullptr, &kp, (long)key.len);
	const unsigned char *cp = (const unsigned char *)cert.buf;
	store.chain = sk_X509_new_null();
	sk_X509_push(store.chain, d2i_X509(nullptr, &cp, (long)cert.len));
	SSL_CTX *sctx = tls_server_ctx(&store, "irc", nullptr, nullptr);
	struct conn_options opts = {.tls_server = sctx, .proxy = true, .setup_timeout_ms = 5000};
	for (size_t k = 0; k <= hello.len; k++) {
		struct peer p = run(strbuf_str(&hello), k, &opts);
		assert_false(p.ready);
		assert_int_equal(p.messages, 0);
	}
	// Plain text to a TLS listener
	struct peer p = run(STR_LIT("NICK a\r\nUSER a 0 * :a\r\n"), 23, &opts);
	assert_false(p.ready);
	SSL_CTX_free(sctx);
	tls_cert_store_finish(&store);
	strbuf_free(&hello);
	strbuf_free(&key);
	strbuf_free(&cert);
}

T_MAIN(cmocka_unit_test(test_disconnect_at_every_byte),
       cmocka_unit_test(test_tls_truncated_handshake))
