// Risk 3: the vhost callback exposes its SSL_CTX. Replace certificates on
// that same context, then negotiate HTTP/1.1 and accept an untrusted client
// certificate using memory BIOs, without SNI or any listening sockets.
#include <assert.h>
#include <libwebsockets.h>
#include <openssl/err.h>
#include <openssl/pem.h>
#include <openssl/ssl.h>
#include <string.h>
#include <uv.h>

#include "support.h"

static SSL_CTX *server_ctx;
static EVP_PKEY *server_key;
static X509 *server_cert;
static int callback(struct lws *wsi, enum lws_callback_reasons reason, void *user, void *in,
		    size_t len) {
	(void)wsi;
	(void)in;
	(void)len;
	if (reason == LWS_CALLBACK_OPENSSL_LOAD_EXTRA_SERVER_VERIFY_CERTS) {
		server_ctx = user; // borrowed for the lifetime of the vhost
		assert(SSL_CTX_use_certificate(server_ctx, server_cert) == 1);
		assert(SSL_CTX_use_PrivateKey(server_ctx, server_key) == 1);
	}
	return 0;
}

static int accept_cert(int valid, X509_STORE_CTX *ctx) {
	(void)valid;
	(void)ctx;
	return 1;
}

static int alpn(SSL *ssl, const unsigned char **out, unsigned char *outlen, const unsigned char *in,
		unsigned int inlen, void *arg) {
	(void)ssl;
	(void)arg;
	static constexpr unsigned char protocols[] = "\x08http/1.1";
	unsigned char *selected = nullptr;
	int rc = SSL_select_next_proto(&selected, outlen, protocols, sizeof(protocols) - 1, in,
				       inlen);
	*out = selected;
	return rc == OPENSSL_NPN_NEGOTIATED ? SSL_TLSEXT_ERR_OK : SSL_TLSEXT_ERR_ALERT_FATAL;
}

static X509 *certificate(EVP_PKEY *key, long serial) {
	X509 *cert = X509_new();
	assert(cert != nullptr);
	assert(X509_set_version(cert, 2) == 1);
	assert(ASN1_INTEGER_set(X509_get_serialNumber(cert), serial) == 1);
	assert(X509_gmtime_adj(X509_getm_notBefore(cert), -60) != nullptr);
	assert(X509_gmtime_adj(X509_getm_notAfter(cert), 3600) != nullptr);
	assert(X509_set_pubkey(cert, key) == 1);
	X509_NAME *name = X509_NAME_new();
	assert(name != nullptr);
	assert(X509_NAME_add_entry_by_txt(name, "CN", MBSTRING_ASC,
					  (const unsigned char *)"localhost", -1, -1, 0) == 1);
	assert(X509_set_issuer_name(cert, name) == 1);
	assert(X509_set_subject_name(cert, name) == 1);
	X509_NAME_free(name);
	assert(X509_sign(cert, key, nullptr) > 0);
	return cert;
}

int main(void) {
	EVP_PKEY *key = EVP_PKEY_Q_keygen(nullptr, nullptr, "ED25519");
	assert(key != nullptr);
	X509 *cert = certificate(key, 1);
	server_key = key;
	server_cert = cert;
	uv_loop_t loop;
	assert(uv_loop_init(&loop) == 0);
	void *loops[] = {&loop};
	struct lws_context *context = nullptr;
	const struct lws_protocols protocols[] = {{.name = "http", .callback = callback}, {0}};
	struct lws_context_creation_info info = {
		.port = CONTEXT_PORT_NO_LISTEN_SERVER,
		.protocols = protocols,
		.options = LWS_SERVER_OPTION_LIBUV | LWS_SERVER_OPTION_DO_SSL_GLOBAL_INIT |
			   LWS_SERVER_OPTION_UV_NO_SIGSEGV_SIGFPE_SPIN |
			   LWS_SERVER_OPTION_CREATE_VHOST_SSL_CTX,
		.foreign_loops = loops,
		.pcontext = &context,
		.alpn = "http/1.1",
	};
	context = lws_create_context(&info);
	assert(context != nullptr && server_ctx != nullptr);
	X509 *replacement = certificate(key, 2);
	assert(SSL_CTX_use_certificate(server_ctx, replacement) == 1);
	assert(SSL_CTX_check_private_key(server_ctx) == 1);
	SSL_CTX_set_verify(server_ctx, SSL_VERIFY_PEER | SSL_VERIFY_FAIL_IF_NO_PEER_CERT,
			   accept_cert);
	SSL_CTX_set_alpn_select_cb(server_ctx, alpn, nullptr);
	SSL_CTX *client_ctx = SSL_CTX_new(TLS_client_method());
	assert(client_ctx != nullptr);
	assert(SSL_CTX_use_certificate(client_ctx, cert) == 1);
	assert(SSL_CTX_use_PrivateKey(client_ctx, key) == 1);
	SSL *client = SSL_new(client_ctx), *server = SSL_new(server_ctx);
	assert(client != nullptr && server != nullptr);
	BIO *a, *b;
	assert(BIO_new_bio_pair(&a, 0, &b, 0) == 1);
	SSL_set_bio(client, a, a);
	SSL_set_bio(server, b, b);
	SSL_set_connect_state(client);
	SSL_set_accept_state(server);
	constexpr unsigned char offered[] = "\x02h2\x08http/1.1";
	assert(SSL_set_alpn_protos(client, offered, sizeof(offered) - 1) == 0);
	for (int i = 0; i < 100 && (!SSL_is_init_finished(client) || !SSL_is_init_finished(server));
	     i++) {
		int rc = SSL_do_handshake(client);
		if (rc != 1 && SSL_get_error(client, rc) != SSL_ERROR_WANT_READ) {
			ERR_print_errors_fp(stderr);
		}
		assert(rc == 1 || SSL_get_error(client, rc) == SSL_ERROR_WANT_READ);
		rc = SSL_do_handshake(server);
		if (rc != 1 && SSL_get_error(server, rc) != SSL_ERROR_WANT_READ) {
			ERR_print_errors_fp(stderr);
		}
		assert(rc == 1 || SSL_get_error(server, rc) == SSL_ERROR_WANT_READ);
	}
	assert(SSL_is_init_finished(client) && SSL_is_init_finished(server));
	const unsigned char *selected;
	unsigned int selected_len;
	SSL_get0_alpn_selected(client, &selected, &selected_len);
	assert(selected_len == 8 && memcmp(selected, "http/1.1", 8) == 0);
	X509 *peer = SSL_get1_peer_certificate(client);
	assert(peer != nullptr && ASN1_INTEGER_get(X509_get_serialNumber(peer)) == 2);
	X509_free(peer);
	peer = SSL_get1_peer_certificate(server);
	assert(peer != nullptr);
	X509_free(peer);
	SSL_free(client);
	SSL_free(server);
	SSL_CTX_free(client_ctx);
	spike_shutdown(&context, &loop);
	X509_free(cert);
	X509_free(replacement);
	EVP_PKEY_free(key);
	OPENSSL_cleanup();
	return 0;
}
