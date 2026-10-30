#include "net/tls.h"

#include <openssl/err.h>
#include <openssl/pem.h>
#include <openssl/x509v3.h>
#include <stdio.h>
#include <string.h>

#include "base/alloc.h"
#include "net/ipaddr.h"

void tls_cert_store_init(struct tls_cert_store *s) {
	*s = (struct tls_cert_store){0};
	pthread_mutex_init(&s->mu, nullptr);
}

void tls_cert_store_finish(struct tls_cert_store *s) {
	sk_X509_pop_free(s->chain, X509_free);
	EVP_PKEY_free(s->key);
	pthread_mutex_destroy(&s->mu);
}

static void openssl_error(char *buf, size_t n) {
	unsigned long e = ERR_get_error();
	if (e == 0) {
		snprintf(buf, n, "unknown error");
	} else {
		ERR_error_string_n(e, buf, n);
	}
	ERR_clear_error();
}

bool tls_cert_store_load(struct tls_cert_store *s, const char *cert_path, const char *key_path,
			 struct err **err) {
	char msg[256];
	FILE *f = fopen(cert_path, "r");
	if (f == nullptr) {
		return err_set(err, 0, "failed to read %s: %s", cert_path, strerror(errno));
	}
	STACK_OF(X509) *chain = sk_X509_new_null();
	X509 *x;
	while ((x = PEM_read_X509(f, nullptr, nullptr, nullptr)) != nullptr) {
		sk_X509_push(chain, x);
	}
	fclose(f);
	unsigned long e = ERR_peek_last_error();
	if (ERR_GET_REASON(e) != PEM_R_NO_START_LINE || sk_X509_num(chain) == 0) {
		bool empty = sk_X509_num(chain) == 0 && ERR_GET_REASON(e) == PEM_R_NO_START_LINE;
		openssl_error(msg, sizeof(msg));
		sk_X509_pop_free(chain, X509_free);
		if (empty) {
			return err_set(err, 0, "no certificate found in %s", cert_path);
		}
		return err_set(err, 0, "failed to parse %s: %s", cert_path, msg);
	}
	ERR_clear_error();
	f = fopen(key_path, "r");
	if (f == nullptr) {
		sk_X509_pop_free(chain, X509_free);
		return err_set(err, 0, "failed to read private key from %s: %s", key_path,
			       strerror(errno));
	}
	EVP_PKEY *key = PEM_read_PrivateKey(f, nullptr, nullptr, nullptr);
	fclose(f);
	if (key == nullptr) {
		openssl_error(msg, sizeof(msg));
		sk_X509_pop_free(chain, X509_free);
		return err_set(err, 0, "failed to read private key from %s: %s", key_path, msg);
	}
	if (X509_check_private_key(sk_X509_value(chain, 0), key) != 1) {
		openssl_error(msg, sizeof(msg));
		sk_X509_pop_free(chain, X509_free);
		EVP_PKEY_free(key);
		return err_set(err, 0, "unsupported private key: %s", msg);
	}
	pthread_mutex_lock(&s->mu);
	STACK_OF(X509) *old_chain = s->chain;
	EVP_PKEY *old_key = s->key;
	s->chain = chain;
	s->key = key;
	pthread_mutex_unlock(&s->mu);
	sk_X509_pop_free(old_chain, X509_free);
	EVP_PKEY_free(old_key);
	return true;
}

struct pinned {
	char *algo;
	char *fingerprint;
};

struct server_ctx_data {
	struct tls_cert_store *store;
	char alpn[16];
	const bool *client_cert_auth;
	const atomic_bool *atomic_client_cert_auth;
};

static int ctx_data_index = -1;
static int ctx_pinned_index = -1;
static int ssl_data_index = -1;
static pthread_once_t index_once = PTHREAD_ONCE_INIT;

static void free_ctx_data(void *parent, void *ptr, CRYPTO_EX_DATA *ad, int idx, long argl,
			  void *argp) {
	(void)parent;
	(void)ad;
	(void)idx;
	(void)argl;
	(void)argp;
	xfree(ptr);
}

static void free_pinned(void *parent, void *ptr, CRYPTO_EX_DATA *ad, int idx, long argl,
			void *argp) {
	(void)parent;
	(void)ad;
	(void)idx;
	(void)argl;
	(void)argp;
	struct pinned *p = ptr;
	if (p != nullptr) {
		xfree(p->algo);
		xfree(p->fingerprint);
		xfree(p);
	}
}

static void init_indexes(void) {
	ctx_data_index = SSL_CTX_get_ex_new_index(0, nullptr, nullptr, nullptr, free_ctx_data);
	ctx_pinned_index = SSL_CTX_get_ex_new_index(0, nullptr, nullptr, nullptr, free_pinned);
	ssl_data_index = SSL_get_ex_new_index(0, nullptr, nullptr, nullptr, nullptr);
}

// Accepts any client certificate: soju checks its fingerprint later
static int accept_any(int preverify, X509_STORE_CTX *ctx) {
	(void)preverify;
	(void)ctx;
	return 1;
}

// Installs the current certificate for each handshake, so that a reload
// applies to new connections without recreating the context
static int cert_cb(SSL *ssl, void *arg) {
	struct server_ctx_data *d = arg;
	pthread_mutex_lock(&d->store->mu);
	int ok = 0;
	if (d->store->chain != nullptr) {
		X509 *leaf = sk_X509_value(d->store->chain, 0);
		STACK_OF(X509) *extra = sk_X509_new_null();
		for (int i = 1; i < sk_X509_num(d->store->chain); i++) {
			sk_X509_push(extra, sk_X509_value(d->store->chain, i));
		}
		ok = SSL_use_cert_and_key(ssl, leaf, d->store->key, extra, 1);
		sk_X509_free(extra);
	}
	pthread_mutex_unlock(&d->store->mu);
	bool request = d->atomic_client_cert_auth != nullptr
			       ? atomic_load(d->atomic_client_cert_auth)
			       : d->client_cert_auth != nullptr && *d->client_cert_auth;
	if (ok && request) {
		SSL_set_verify(ssl, SSL_VERIFY_PEER, accept_any);
	}
	return ok;
}

static int alpn_select(SSL *ssl, const unsigned char **out, unsigned char *outlen,
		       const unsigned char *in, unsigned inlen, void *arg) {
	(void)ssl;
	const struct server_ctx_data *d = arg;
	size_t n = strlen(d->alpn);
	for (unsigned i = 0; i < inlen;) {
		unsigned l = in[i];
		if (i + 1 + l > inlen) {
			break;
		}
		if (l == n && memcmp(in + i + 1, d->alpn, n) == 0) {
			*out = in + i + 1;
			*outlen = (unsigned char)l;
			return SSL_TLSEXT_ERR_OK;
		}
		i += 1 + l;
	}
	// Offered protocols, none of ours: rustls rejects the handshake
	return SSL_TLSEXT_ERR_ALERT_FATAL;
}

static void set_versions(SSL_CTX *ctx) {
	// rustls's safe defaults
	SSL_CTX_set_min_proto_version(ctx, TLS1_2_VERSION);
	SSL_CTX_set_max_proto_version(ctx, TLS1_3_VERSION);
	SSL_CTX_set_options(ctx, SSL_OP_NO_RENEGOTIATION | SSL_OP_NO_COMPRESSION);
}

SSL_CTX *tls_server_ctx(struct tls_cert_store *store, const char *alpn,
			const bool *client_cert_auth, struct err **err) {
	pthread_once(&index_once, init_indexes);
	SSL_CTX *ctx = SSL_CTX_new(TLS_server_method());
	if (ctx == nullptr) {
		err_set(err, 0, "failed to create a TLS context");
		return nullptr;
	}
	set_versions(ctx);
	struct server_ctx_data *d = xcalloc(1, sizeof(*d));
	d->store = store;
	snprintf(d->alpn, sizeof(d->alpn), "%s", alpn);
	d->client_cert_auth = client_cert_auth;
	SSL_CTX_set_ex_data(ctx, ctx_data_index, d);
	SSL_CTX_set_cert_cb(ctx, cert_cb, d);
	SSL_CTX_set_alpn_select_cb(ctx, alpn_select, d);
	SSL_CTX_set_session_cache_mode(ctx, SSL_SESS_CACHE_OFF);
	SSL_CTX_set_num_tickets(ctx, 0);
	return ctx;
}

static void hex_digest(const EVP_MD *md, const unsigned char *der, int len, char *out,
		       size_t outlen) {
	unsigned char d[EVP_MAX_MD_SIZE];
	unsigned n = 0;
	EVP_Digest(der, (size_t)len, d, &n, md, nullptr);
	size_t j = 0;
	for (unsigned i = 0; i < n && j + 2 < outlen; i++) {
		j += (size_t)snprintf(out + j, outlen - j, "%02x", d[i]);
	}
	out[j] = '\0';
}

// Checks only the leaf against the pinned fingerprint: the handshake
// signature proves possession of the leaf key, but nothing links the leaf
// to the other certificates the server sent, so matching those would let
// anyone append the pinned (public) certificate to their own chain
static int verify_pinned(int preverify, X509_STORE_CTX *xs) {
	(void)preverify;
	if (X509_STORE_CTX_get_error_depth(xs) != 0) {
		return 1;
	}
	SSL *ssl = X509_STORE_CTX_get_ex_data(xs, SSL_get_ex_data_X509_STORE_CTX_idx());
	SSL_CTX *ctx = SSL_get_SSL_CTX(ssl);
	const struct pinned *p = SSL_CTX_get_ex_data(ctx, ctx_pinned_index);
	X509 *leaf = X509_STORE_CTX_get_current_cert(xs);
	unsigned char *der = nullptr;
	int len = i2d_X509(leaf, &der);
	if (len <= 0) {
		return 0;
	}
	char remote[129] = "";
	if (cstr_eq(p->algo, "sha-512")) {
		hex_digest(EVP_sha512(), der, len, remote, sizeof(remote));
	} else if (cstr_eq(p->algo, "sha-256")) {
		hex_digest(EVP_sha256(), der, len, remote, sizeof(remote));
	}
	bool ok = remote[0] != '\0' && cstr_eq(remote, p->fingerprint);
	if (!ok) {
		// Give the user a fingerprint they can use to connect
		hex_digest(EVP_sha512(), der, len, remote, sizeof(remote));
		struct tls_session *t = SSL_get_ex_data(ssl, ssl_data_index);
		if (t != nullptr) {
			snprintf(t->error, sizeof(t->error),
				 "the configured TLS certificate fingerprint doesn't match the "
				 "server's - %s",
				 remote);
		}
	}
	OPENSSL_free(der);
	return ok;
}

SSL_CTX *tls_client_ctx(const char *certfp, struct str cert, struct str key, struct err **err) {
	pthread_once(&index_once, init_indexes);
	SSL_CTX *ctx = SSL_CTX_new(TLS_client_method());
	if (ctx == nullptr) {
		err_set(err, 0, "failed to create a TLS context");
		return nullptr;
	}
	set_versions(ctx);
	if (certfp[0] == '\0') {
		SSL_CTX_set_default_verify_paths(ctx);
		SSL_CTX_set_verify(ctx, SSL_VERIFY_PEER, nullptr);
	} else {
		struct pinned *p = xcalloc(1, sizeof(*p));
		struct str algo, fp;
		if (!str_split_once(str_from(certfp), ':', &algo, &fp)) {
			algo = STR_EMPTY;
			fp = str_from(certfp);
		}
		p->algo = str_dup(algo);
		p->fingerprint = str_dup(fp);
		SSL_CTX_set_ex_data(ctx, ctx_pinned_index, p);
		SSL_CTX_set_verify(ctx, SSL_VERIFY_PEER, verify_pinned);
	}
	if (cert.len > 0) {
		const unsigned char *kp = (const unsigned char *)key.p;
		EVP_PKEY *pk = d2i_AutoPrivateKey(nullptr, &kp, (long)key.len);
		char msg[256];
		if (pk == nullptr ||
		    SSL_CTX_use_certificate_ASN1(ctx, (int)cert.len,
						 (const unsigned char *)cert.p) != 1 ||
		    SSL_CTX_use_PrivateKey(ctx, pk) != 1) {
			openssl_error(msg, sizeof(msg));
			EVP_PKEY_free(pk);
			SSL_CTX_free(ctx);
			err_set(err, 0, "failed to parse private key: %s", msg);
			return nullptr;
		}
		EVP_PKEY_free(pk);
	}
	static const unsigned char alpn[] = "\x03irc";
	SSL_CTX_set_alpn_protos(ctx, alpn, sizeof(alpn) - 1);
	return ctx;
}

SSL_CTX *tls_http_client_ctx(struct err **err) {
	SSL_CTX *ctx = tls_client_ctx("", STR_EMPTY, STR_EMPTY, err);
	if (ctx != nullptr) {
		static const unsigned char alpn[] = "\x08http/1.1";
		SSL_CTX_set_alpn_protos(ctx, alpn, sizeof(alpn) - 1);
	}
	return ctx;
}

static bool session_init(struct tls_session *t, SSL_CTX *ctx) {
	*t = (struct tls_session){0};
	t->ssl = SSL_new(ctx);
	t->in = BIO_new(BIO_s_mem());
	t->out = BIO_new(BIO_s_mem());
	if (t->ssl == nullptr || t->in == nullptr || t->out == nullptr) {
		SSL_free(t->ssl);
		BIO_free(t->in);
		BIO_free(t->out);
		*t = (struct tls_session){0};
		return false;
	}
	BIO_set_mem_eof_return(t->in, -1);
	SSL_set_bio(t->ssl, t->in, t->out);
	SSL_set_ex_data(t->ssl, ssl_data_index, t);
	return true;
}

bool tls_session_init_server(struct tls_session *t, SSL_CTX *ctx) {
	if (!session_init(t, ctx)) {
		return false;
	}
	SSL_set_accept_state(t->ssl);
	return true;
}

bool tls_session_init_client(struct tls_session *t, SSL_CTX *ctx, const char *host) {
	if (!session_init(t, ctx)) {
		return false;
	}
	struct ipaddr ip;
	bool is_ip = ipaddr_parse(str_from(host), &ip);
	if (!is_ip) {
		// The macro casts away const: give it a copy
		char *h = xstrdup(host);
		SSL_set_tlsext_host_name(t->ssl, h);
		xfree(h);
	}
	if (SSL_get_verify_callback(t->ssl) == nullptr) {
		// CA verification: check the name too
		if (is_ip) {
			X509_VERIFY_PARAM_set1_ip_asc(SSL_get0_param(t->ssl), host);
		} else {
			SSL_set1_host(t->ssl, host);
		}
	}
	SSL_set_connect_state(t->ssl);
	return true;
}

void tls_session_finish(struct tls_session *t) {
	SSL_free(t->ssl); // frees the BIOs
	*t = (struct tls_session){0};
}

void tls_session_feed(struct tls_session *t, const void *data, size_t len) {
	if (len > 0) {
		BIO_write(t->in, data, (int)len);
	}
}

static enum tls_status status(struct tls_session *t, int rc) {
	switch (SSL_get_error(t->ssl, rc)) {
	case SSL_ERROR_WANT_READ:
	case SSL_ERROR_WANT_WRITE:
		ERR_clear_error();
		return TLS_WANT_INPUT;
	case SSL_ERROR_ZERO_RETURN:
		ERR_clear_error();
		return TLS_CLOSED;
	default:
		if (t->error[0] == '\0') {
			long v = SSL_get_verify_result(t->ssl);
			if (v != X509_V_OK) {
				snprintf(t->error, sizeof(t->error), "invalid peer certificate: %s",
					 X509_verify_cert_error_string(v));
			} else {
				openssl_error(t->error, sizeof(t->error));
			}
		}
		ERR_clear_error();
		return TLS_ERROR;
	}
}

enum tls_status tls_session_handshake(struct tls_session *t) {
	if (t->handshake_done) {
		return TLS_OK;
	}
	int rc = SSL_do_handshake(t->ssl);
	if (rc == 1) {
		t->handshake_done = true;
		return TLS_OK;
	}
	return status(t, rc);
}

enum tls_status tls_session_read(struct tls_session *t, struct strbuf *out) {
	char buf[16384];
	while (true) {
		size_t n = 0;
		int rc = SSL_read_ex(t->ssl, buf, sizeof(buf), &n);
		if (rc == 1) {
			strbuf_append(out, buf, n);
			continue;
		}
		enum tls_status st = status(t, 0);
		return st == TLS_WANT_INPUT ? TLS_OK : st;
	}
}

enum tls_status tls_session_write(struct tls_session *t, const void *data, size_t len) {
	size_t written = 0;
	if (len == 0) {
		return TLS_OK;
	}
	int rc = SSL_write_ex(t->ssl, data, len, &written);
	if (rc == 1) {
		return TLS_OK;
	}
	return status(t, 0);
}

void tls_session_shutdown(struct tls_session *t) {
	if (t->handshake_done) {
		(void)SSL_shutdown(t->ssl);
		ERR_clear_error();
	}
}

bool tls_session_take_output(struct tls_session *t, struct strbuf *out) {
	char *p;
	long n = BIO_get_mem_data(t->out, &p);
	if (n <= 0) {
		return false;
	}
	strbuf_append(out, p, (size_t)n);
	(void)BIO_reset(t->out);
	return true;
}

bool tls_session_peer_cert(struct tls_session *t, struct strbuf *out) {
	X509 *x = SSL_get0_peer_certificate(t->ssl);
	if (x == nullptr) {
		return false;
	}
	unsigned char *der = nullptr;
	int n = i2d_X509(x, &der);
	if (n <= 0) {
		return false;
	}
	strbuf_append(out, (const char *)der, (size_t)n);
	OPENSSL_free(der);
	return true;
}

struct str tls_session_alpn(const struct tls_session *t) {
	const unsigned char *p = nullptr;
	unsigned n = 0;
	SSL_get0_alpn_selected(t->ssl, &p, &n);
	return p != nullptr ? str_new((const char *)p, n) : STR_EMPTY;
}

void tls_server_ctx_set_client_cert_auth(SSL_CTX *ctx, const atomic_bool *enabled) {
	struct server_ctx_data *data = SSL_CTX_get_ex_data(ctx, ctx_data_index);
	data->atomic_client_cert_auth = enabled;
}
