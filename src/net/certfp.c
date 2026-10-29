#include "net/certfp.h"

#include <openssl/evp.h>
#include <openssl/rand.h>
#include <openssl/x509.h>
#include <openssl/x509v3.h>

#include "base/alloc.h"

static EVP_PKEY *generate_key(const char *key_type, unsigned bits, struct err **err) {
	if (cstr_eq(key_type, "rsa")) {
		if (bits != 2048 && bits != 3072 && bits != 4096 && bits != 8192) {
			err_set(err, 0,
				"unsupported RSA key size %u (supported sizes: 2048, 3072, 4096, "
				"8192)",
				bits);
			return nullptr;
		}
		EVP_PKEY *k = EVP_RSA_gen(bits);
		if (k == nullptr) {
			err_set(err, 0, "failed to generate RSA key");
		}
		return k;
	}
	if (cstr_eq(key_type, "ecdsa")) {
		// EVP_EC_gen casts away const
		char curve[] = "P-521";
		EVP_PKEY *k = EVP_PKEY_Q_keygen(nullptr, nullptr, "EC", curve);
		if (k == nullptr) {
			err_set(err, 0, "failed to generate ECDSA key");
		}
		return k;
	}
	if (cstr_eq(key_type, "ed25519")) {
		EVP_PKEY *k = EVP_PKEY_Q_keygen(nullptr, nullptr, "ED25519");
		if (k == nullptr) {
			err_set(err, 0, "failed to generate Ed25519 key");
		}
		return k;
	}
	// Same error as upstream, which fails to marshal a nil key
	err_set(err, 0, "x509: unknown key type while marshaling PKCS#8: <nil>");
	return nullptr;
}

static void append_der(struct strbuf *out, const unsigned char *p, int n) {
	strbuf_append(out, (const char *)p, (size_t)n);
	strbuf_append(out, "", 0);
}

bool generate_certfp(const char *key_type, unsigned bits, struct strbuf *key, struct strbuf *cert,
		     struct err **err) {
	EVP_PKEY *pkey = generate_key(key_type, bits, err);
	if (pkey == nullptr) {
		return false;
	}
	bool ok = false;
	X509 *x = X509_new();
	unsigned char serial[16];
	BIGNUM *bn = nullptr;
	unsigned char *der = nullptr;
	int n;
	if (x == nullptr || RAND_bytes(serial, sizeof(serial)) != 1) {
		goto out;
	}
	serial[0] = (unsigned char)(serial[0] & 0x7FU);
	bn = BN_bin2bn(serial, sizeof(serial), nullptr);
	if (bn == nullptr || BN_to_ASN1_INTEGER(bn, X509_get_serialNumber(x)) == nullptr ||
	    !X509_set_version(x, X509_VERSION_3)) {
		goto out;
	}
	X509_NAME *name = X509_get_subject_name(x);
	if (!X509_NAME_add_entry_by_txt(name, "CN", MBSTRING_UTF8,
					(const unsigned char *)"soju auto-generated certificate",
					-1, -1, 0) ||
	    !X509_set_issuer_name(x, name)) {
		goto out;
	}
	// Let's make a fair assumption nobody will use the same cert for more
	// than 20 years...
	if (X509_gmtime_adj(X509_getm_notBefore(x), 0) == nullptr ||
	    X509_gmtime_adj(X509_getm_notAfter(x), 60L * 60 * 24 * 365 * 20) == nullptr ||
	    !X509_set_pubkey(x, pkey)) {
		goto out;
	}
	X509V3_CTX ctx;
	X509V3_set_ctx_nodb(&ctx);
	X509V3_set_ctx(&ctx, x, x, nullptr, nullptr, 0);
	X509_EXTENSION *ku = X509V3_EXT_conf_nid(nullptr, &ctx, NID_key_usage,
						 "digitalSignature,keyEncipherment");
	X509_EXTENSION *eku = X509V3_EXT_conf_nid(nullptr, &ctx, NID_ext_key_usage, "clientAuth");
	bool ext_ok = ku != nullptr && eku != nullptr && X509_add_ext(x, ku, -1) &&
		      X509_add_ext(x, eku, -1);
	X509_EXTENSION_free(ku);
	X509_EXTENSION_free(eku);
	if (!ext_ok) {
		goto out;
	}
	// Ed25519 signs without a separate digest
	const EVP_MD *md = cstr_eq(key_type, "ed25519") ? nullptr
			   : cstr_eq(key_type, "ecdsa") ? EVP_sha512()
							: EVP_sha256();
	if (X509_sign(x, pkey, md) == 0) {
		goto out;
	}
	PKCS8_PRIV_KEY_INFO *p8 = EVP_PKEY2PKCS8(pkey);
	if (p8 == nullptr) {
		goto out;
	}
	n = i2d_PKCS8_PRIV_KEY_INFO(p8, &der);
	PKCS8_PRIV_KEY_INFO_free(p8);
	if (n <= 0) {
		goto out;
	}
	append_der(key, der, n);
	OPENSSL_clear_free(der, (size_t)n);
	der = nullptr;
	n = i2d_X509(x, &der);
	if (n <= 0) {
		goto out;
	}
	append_der(cert, der, n);
	ok = true;
out:
	if (!ok) {
		err_set(err, 0, "failed to generate a certificate");
	}
	OPENSSL_free(der);
	BN_free(bn);
	X509_free(x);
	EVP_PKEY_free(pkey);
	return ok;
}
