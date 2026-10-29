// TLS for listeners and upstream connections with OpenSSL (soju-rs tls.rs
// used rustls): server certificates swappable at run time, optional client
// certificates, ALPN, pinned upstream fingerprints, and a TLS session over
// memory buffers to run on libuv streams.
#pragma once

#include <openssl/ssl.h>
#include <pthread.h>

#include "base/err.h"
#include "base/str.h"

// A server certificate which can be swapped at run time (on SIGHUP),
// shared by the loop threads
struct tls_cert_store {
	pthread_mutex_t mu;
	STACK_OF(X509) * chain; // leaf first; nullptr until set
	EVP_PKEY *key;
};

void tls_cert_store_init(struct tls_cert_store *s);
void tls_cert_store_finish(struct tls_cert_store *s);
// Loads a PEM certificate chain and private key into the store
[[nodiscard]] bool tls_cert_store_load(struct tls_cert_store *s, const char *cert_path,
				       const char *key_path, struct err **err);

// A server context for a listener: alpn is "irc" or "http/1.1". Clients
// offering ALPN protocols but not this one are rejected during the
// handshake. When *client_cert_auth is true at handshake time, a client
// certificate is requested but not verified (like Go's RequestClientCert).
[[nodiscard]] SSL_CTX *tls_server_ctx(struct tls_cert_store *store, const char *alpn,
				      const bool *client_cert_auth, struct err **err);

// A client context for an upstream: certfp is the pinned fingerprint
// ("sha-256:<hex>" or "sha-512:<hex>"), or "" to verify against the system
// trust store. cert/key: DER certificate and PKCS#8 key for SASL EXTERNAL
// (cert.len == 0: none). ALPN "irc".
[[nodiscard]] SSL_CTX *tls_client_ctx(const char *certfp, struct str cert, struct str key,
				      struct err **err);

enum tls_status {
	TLS_OK, // progress made (or nothing to do)
	TLS_WANT_INPUT, // more ciphertext needed
	TLS_CLOSED, // the peer closed the session cleanly
	TLS_ERROR,
};

// A TLS session over memory buffers: ciphertext goes in with
// tls_session_feed and out with tls_session_take_output
struct tls_session {
	SSL *ssl;
	BIO *in; // ciphertext from the peer
	BIO *out; // ciphertext to the peer
	bool handshake_done;
	char error[256];
};

[[nodiscard]] bool tls_session_init_server(struct tls_session *t, SSL_CTX *ctx);
// host: the server name (SNI and certificate verification; an IP address
// is verified but not sent as SNI)
[[nodiscard]] bool tls_session_init_client(struct tls_session *t, SSL_CTX *ctx, const char *host);
void tls_session_finish(struct tls_session *t);
void tls_session_feed(struct tls_session *t, const void *data, size_t len);
// Advances the handshake; TLS_OK once it's done
[[nodiscard]] enum tls_status tls_session_handshake(struct tls_session *t);
// Appends decrypted data to out
[[nodiscard]] enum tls_status tls_session_read(struct tls_session *t, struct strbuf *out);
[[nodiscard]] enum tls_status tls_session_write(struct tls_session *t, const void *data,
						size_t len);
// Queues a close_notify
void tls_session_shutdown(struct tls_session *t);
// Moves the pending ciphertext to out; returns whether there was any
bool tls_session_take_output(struct tls_session *t, struct strbuf *out);
// The DER of the peer's leaf certificate, appended to out; false if none
bool tls_session_peer_cert(struct tls_session *t, struct strbuf *out);
// The negotiated ALPN protocol, or ""
[[nodiscard]] struct str tls_session_alpn(const struct tls_session *t);
