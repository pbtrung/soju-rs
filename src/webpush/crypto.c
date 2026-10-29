// Web Push cryptography (soju-rs webpush.rs) with OpenSSL: P-256 keys,
// ECDH, HKDF-SHA-256, AES-128-GCM (RFC 8291) and ES256 JWTs (RFC 8292)
#include <openssl/core_names.h>
#include <openssl/ec.h>
#include <openssl/evp.h>
#include <openssl/kdf.h>
#include <openssl/param_build.h>
#include <openssl/rand.h>
#include <string.h>
#include <time.h>
#include <yyjson.h>

#include "base/alloc.h"
#include "base/base64.h"
#include "net/url.h"
#include "webpush/internal.h"

static void *unconst(const void *p) {
	return (void *)(uintptr_t)p;
}

EVP_PKEY *webpush_generate_key(void) {
	EVP_PKEY *k = EVP_PKEY_Q_keygen(nullptr, nullptr, "EC", "P-256");
	if (k == nullptr) {
		die_oom();
	}
	return k;
}

bool webpush_public_bytes(EVP_PKEY *k, uint8_t out[65]) {
	size_t len = 0;
	return EVP_PKEY_get_octet_string_param(k, OSSL_PKEY_PARAM_PUB_KEY, out, 65, &len) == 1 &&
	       len == 65;
}

bool webpush_private_bytes(EVP_PKEY *k, uint8_t out[32]) {
	BIGNUM *bn = nullptr;
	if (EVP_PKEY_get_bn_param(k, OSSL_PKEY_PARAM_PRIV_KEY, &bn) != 1) {
		return false;
	}
	bool ok = BN_bn2binpad(bn, out, 32) == 32;
	BN_clear_free(bn);
	return ok;
}

// A P-256 key from its parts: pub (SEC1) and/or priv (32 bytes)
static EVP_PKEY *key_from(const uint8_t *pub, size_t pub_len, const uint8_t *priv) {
	OSSL_PARAM_BLD *b = OSSL_PARAM_BLD_new();
	BIGNUM *bn = nullptr;
	OSSL_PARAM *params = nullptr;
	EVP_PKEY *k = nullptr;
	EVP_PKEY_CTX *ctx = nullptr;
	if (b == nullptr ||
	    OSSL_PARAM_BLD_push_utf8_string(b, OSSL_PKEY_PARAM_GROUP_NAME, "prime256v1", 0) != 1) {
		goto out;
	}
	if (pub != nullptr &&
	    OSSL_PARAM_BLD_push_octet_string(b, OSSL_PKEY_PARAM_PUB_KEY, pub, pub_len) != 1) {
		goto out;
	}
	if (priv != nullptr) {
		bn = BN_secure_new();
		if (bn == nullptr || BN_bin2bn(priv, 32, bn) == nullptr ||
		    OSSL_PARAM_BLD_push_BN(b, OSSL_PKEY_PARAM_PRIV_KEY, bn) != 1) {
			goto out;
		}
	}
	params = OSSL_PARAM_BLD_to_param(b);
	ctx = EVP_PKEY_CTX_new_from_name(nullptr, "EC", nullptr);
	if (params == nullptr || ctx == nullptr || EVP_PKEY_fromdata_init(ctx) != 1 ||
	    EVP_PKEY_fromdata(ctx, &k, priv != nullptr ? EVP_PKEY_KEYPAIR : EVP_PKEY_PUBLIC_KEY,
			      params) != 1) {
		k = nullptr;
	}
out:
	EVP_PKEY_CTX_free(ctx);
	OSSL_PARAM_free(params);
	BN_clear_free(bn);
	OSSL_PARAM_BLD_free(b);
	return k;
}

EVP_PKEY *webpush_public_key(const uint8_t *pub, size_t len) {
	EVP_PKEY *k = key_from(pub, len, nullptr);
	if (k == nullptr) {
		return nullptr;
	}
	EVP_PKEY_CTX *ctx = EVP_PKEY_CTX_new_from_pkey(nullptr, k, nullptr);
	bool valid = ctx != nullptr && EVP_PKEY_public_check(ctx) == 1;
	EVP_PKEY_CTX_free(ctx);
	if (!valid) {
		EVP_PKEY_free(k);
		return nullptr;
	}
	return k;
}

bool webpush_ecdh(EVP_PKEY *local, EVP_PKEY *remote, uint8_t out[32]) {
	EVP_PKEY_CTX *ctx = EVP_PKEY_CTX_new_from_pkey(nullptr, local, nullptr);
	size_t n = 32;
	bool ok = ctx != nullptr && EVP_PKEY_derive_init(ctx) == 1 &&
		  EVP_PKEY_derive_set_peer(ctx, remote) == 1 &&
		  EVP_PKEY_derive(ctx, out, &n) == 1 && n == 32;
	EVP_PKEY_CTX_free(ctx);
	return ok;
}

bool webpush_hkdf(struct str salt, struct str ikm, struct str info, uint8_t *out, size_t len) {
	EVP_KDF *kdf = EVP_KDF_fetch(nullptr, "HKDF", nullptr);
	EVP_KDF_CTX *ctx = kdf != nullptr ? EVP_KDF_CTX_new(kdf) : nullptr;
	OSSL_PARAM params[] = {
		OSSL_PARAM_construct_utf8_string(OSSL_KDF_PARAM_DIGEST, unconst("SHA256"), 0),
		OSSL_PARAM_construct_octet_string(OSSL_KDF_PARAM_SALT, unconst(salt.p), salt.len),
		OSSL_PARAM_construct_octet_string(OSSL_KDF_PARAM_KEY, unconst(ikm.p), ikm.len),
		OSSL_PARAM_construct_octet_string(OSSL_KDF_PARAM_INFO, unconst(info.p), info.len),
		OSSL_PARAM_construct_end(),
	};
	bool ok = ctx != nullptr && EVP_KDF_derive(ctx, out, len, params) == 1;
	EVP_KDF_CTX_free(ctx);
	EVP_KDF_free(kdf);
	return ok;
}

void webpush_generate_vapid_keys(char **private_key, char **public_key) {
	EVP_PKEY *k = webpush_generate_key();
	uint8_t priv[32], pub[65];
	if (!webpush_private_bytes(k, priv) || !webpush_public_bytes(k, pub)) {
		die_oom();
	}
	EVP_PKEY_free(k);
	*private_key = base64_encode_cstr(BASE64_URL_SAFE_NO_PAD, priv, sizeof(priv));
	*public_key = base64_encode_cstr(BASE64_URL_SAFE_NO_PAD, pub, sizeof(pub));
	OPENSSL_cleanse(priv, sizeof(priv));
}

// A base64 key with padding added if needed: standard base64, else
// URL-safe (soju-rs decode_subscription_key)
static bool decode_subscription_key(const char *key, struct strbuf *out, struct err **err) {
	AUTO_STRBUF struct strbuf s = STRBUF_INIT;
	strbuf_append_cstr(&s, key);
	while (s.len % 4 != 0) {
		strbuf_append_char(&s, '=');
	}
	if (base64_decode(out, BASE64_STANDARD, strbuf_str(&s), nullptr)) {
		return true;
	}
	struct str unpadded = strbuf_str(&s);
	while (unpadded.len > 0 && unpadded.p[unpadded.len - 1] == '=') {
		unpadded.len--;
	}
	struct err *e = nullptr;
	if (base64_decode(out, BASE64_URL_SAFE_NO_PAD, unpadded, &e)) {
		return true;
	}
	err_set(err, 0, "illegal base64 data: %s", err_msg(e));
	err_free(e);
	return false;
}

// URL-safe base64, padded or not (soju-rs decode_vapid_key)
static bool decode_vapid_key(const char *key, struct strbuf *out, struct err **err) {
	struct str s = str_from(key);
	while (s.len > 0 && s.p[s.len - 1] == '=') {
		s.len--;
	}
	struct err *e = nullptr;
	if (base64_decode(out, BASE64_URL_SAFE_NO_PAD, s, &e)) {
		return true;
	}
	err_set(err, 0, "illegal base64 data: %s", err_msg(e));
	err_free(e);
	return false;
}

static bool aes128gcm(const uint8_t key[16], const uint8_t nonce[12], struct str plain,
		      struct strbuf *out) {
	EVP_CIPHER_CTX *c = EVP_CIPHER_CTX_new();
	size_t start = out->len;
	strbuf_reserve(out, size_add(plain.len, 16));
	uint8_t *dst = (uint8_t *)out->buf + start;
	int n = 0, fin = 0;
	bool ok = c != nullptr &&
		  EVP_EncryptInit_ex(c, EVP_aes_128_gcm(), nullptr, nullptr, nullptr) == 1 &&
		  EVP_CIPHER_CTX_ctrl(c, EVP_CTRL_GCM_SET_IVLEN, 12, nullptr) == 1 &&
		  EVP_EncryptInit_ex(c, nullptr, nullptr, key, nonce) == 1 &&
		  EVP_EncryptUpdate(c, dst, &n, (const uint8_t *)plain.p, (int)plain.len) == 1 &&
		  EVP_EncryptFinal_ex(c, dst + n, &fin) == 1 &&
		  EVP_CIPHER_CTX_ctrl(c, EVP_CTRL_GCM_GET_TAG, 16, dst + n + fin) == 1;
	EVP_CIPHER_CTX_free(c);
	if (ok) {
		out->len = start + (size_t)(n + fin) + 16;
		out->buf[out->len] = '\0';
	}
	return ok;
}

bool webpush_encrypt_with_key(struct str message, const char *auth, const char *p256dh,
			      const uint8_t salt[16], EVP_PKEY *local, struct strbuf *out,
			      struct err **err) {
	AUTO_STRBUF struct strbuf auth_secret = STRBUF_INIT, dh = STRBUF_INIT;
	if (!decode_subscription_key(auth, &auth_secret, err) ||
	    !decode_subscription_key(p256dh, &dh, err)) {
		return false;
	}
	EVP_PKEY *remote = webpush_public_key((const uint8_t *)dh.buf, dh.len);
	if (remote == nullptr) {
		return err_set(err, 0,
			       "Unmarshal Error: Public key is not a valid point on the curve");
	}
	uint8_t local_public[65], shared[32];
	bool ok = webpush_public_bytes(local, local_public) && webpush_ecdh(local, remote, shared);
	EVP_PKEY_free(remote);
	if (!ok) {
		return err_set(err, 0, "ECDH failed");
	}
	// RFC 8291 section 3.4: the IKM from the auth secret and the keys
	AUTO_STRBUF struct strbuf info = STRBUF_INIT;
	strbuf_append(&info, "WebPush: info", 14);
	strbuf_append_str(&info, strbuf_str(&dh));
	strbuf_append(&info, (const char *)local_public, 65);
	uint8_t ikm[32], cek[16], nonce[12];
	struct str salt_s = str_new((const char *)salt, 16);
	ok = webpush_hkdf(strbuf_str(&auth_secret), str_new((const char *)shared, 32),
			  strbuf_str(&info), ikm, sizeof(ikm)) &&
	     webpush_hkdf(salt_s, str_new((const char *)ikm, 32),
			  str_new("Content-Encoding: aes128gcm", 28), cek, sizeof(cek)) &&
	     webpush_hkdf(salt_s, str_new((const char *)ikm, 32),
			  str_new("Content-Encoding: nonce", 24), nonce, sizeof(nonce));
	OPENSSL_cleanse(shared, sizeof(shared));
	OPENSSL_cleanse(ikm, sizeof(ikm));
	if (!ok) {
		return err_set(err, 0, "HKDF failed");
	}
	// Header: salt, record size, key ID (the local public key)
	strbuf_append(out, (const char *)salt, 16);
	uint8_t rs[4] = {(uint8_t)(WEBPUSH_RECORD_SIZE >> 24U),
			 (uint8_t)(WEBPUSH_RECORD_SIZE >> 16U),
			 (uint8_t)(WEBPUSH_RECORD_SIZE >> 8U), (uint8_t)WEBPUSH_RECORD_SIZE};
	strbuf_append(out, (const char *)rs, 4);
	strbuf_append_char(out, (char)65);
	strbuf_append(out, (const char *)local_public, 65);
	// A single record, padded: the message, the delimiter, zeros
	size_t max_pad_len = WEBPUSH_RECORD_SIZE - 16 - out->len;
	if (message.len + 1 > max_pad_len) {
		strbuf_reset(out);
		return err_set(err, 0, "payload has exceeded the maximum length");
	}
	AUTO_STRBUF struct strbuf data = STRBUF_INIT;
	strbuf_append_str(&data, message);
	strbuf_append_char(&data, 0x02);
	while (data.len < max_pad_len) {
		strbuf_append_char(&data, '\0');
	}
	ok = aes128gcm(cek, nonce, strbuf_str(&data), out);
	OPENSSL_cleanse(cek, sizeof(cek));
	if (!ok) {
		strbuf_reset(out);
		return err_set(err, 0, "encryption failed");
	}
	return true;
}

bool webpush_encrypt(struct str message, const char *auth, const char *p256dh, struct strbuf *out,
		     struct err **err) {
	uint8_t salt[16];
	if (RAND_bytes(salt, sizeof(salt)) != 1) {
		return err_set(err, 0, "failed to generate a salt");
	}
	EVP_PKEY *local = webpush_generate_key();
	bool ok = webpush_encrypt_with_key(message, auth, p256dh, salt, local, out, err);
	EVP_PKEY_free(local);
	return ok;
}

// ES256: the raw r || s signature of a JWT signing input
static bool sign_es256(EVP_PKEY *k, struct str input, uint8_t out[64]) {
	EVP_MD_CTX *md = EVP_MD_CTX_new();
	uint8_t der[80];
	size_t der_len = sizeof(der);
	bool ok = md != nullptr && EVP_DigestSignInit(md, nullptr, EVP_sha256(), nullptr, k) == 1 &&
		  EVP_DigestSign(md, der, &der_len, (const uint8_t *)input.p, input.len) == 1;
	EVP_MD_CTX_free(md);
	if (!ok) {
		return false;
	}
	const uint8_t *p = der;
	ECDSA_SIG *sig = d2i_ECDSA_SIG(nullptr, &p, (long)der_len);
	if (sig == nullptr) {
		return false;
	}
	const BIGNUM *r, *s;
	ECDSA_SIG_get0(sig, &r, &s);
	ok = BN_bn2binpad(r, out, 32) == 32 && BN_bn2binpad(s, out + 32, 32) == 32;
	ECDSA_SIG_free(sig);
	return ok;
}

bool webpush_vapid_authorization(const char *endpoint, const char *subscriber,
				 const char *public_key, const char *private_key,
				 struct strbuf *out, struct err **err) {
	struct url u;
	if (!url_parse(str_from(endpoint), &u, err)) {
		return false;
	}
	AUTOFREE char *aud = xasprintf("%s://%s", u.scheme, u.host);
	url_free(&u);
	AUTOFREE char *sub = str_starts_with(str_from(subscriber), STR_LIT("https:"))
				     ? xstrdup(subscriber)
				     : xasprintf("mailto:%s", subscriber);
	long long exp = (long long)time(nullptr) + 12LL * 3600;
	yyjson_mut_doc *doc = yyjson_mut_doc_new(nullptr);
	yyjson_mut_val *claims = yyjson_mut_obj(doc);
	yyjson_mut_doc_set_root(doc, claims);
	yyjson_mut_obj_add_str(doc, claims, "aud", aud);
	yyjson_mut_obj_add_int(doc, claims, "exp", exp);
	yyjson_mut_obj_add_str(doc, claims, "sub", sub);
	size_t json_len = 0;
	char *json = yyjson_mut_write(doc, 0, &json_len);
	yyjson_mut_doc_free(doc);
	if (json == nullptr) {
		die_oom();
	}
	AUTO_STRBUF struct strbuf input = STRBUF_INIT;
	static const char header[] = "{\"alg\":\"ES256\",\"typ\":\"JWT\"}";
	base64_encode(&input, BASE64_URL_SAFE_NO_PAD, header, sizeof(header) - 1);
	strbuf_append_char(&input, '.');
	base64_encode(&input, BASE64_URL_SAFE_NO_PAD, json, json_len);
	free(json);

	AUTO_STRBUF struct strbuf priv = STRBUF_INIT, pub = STRBUF_INIT;
	if (!decode_vapid_key(private_key, &priv, err)) {
		return false;
	}
	if (priv.len > 32) {
		return err_set(err, 0, "invalid VAPID private key");
	}
	uint8_t padded[32] = {0};
	memcpy(padded + 32 - priv.len, priv.buf != nullptr ? priv.buf : "", priv.len);
	OPENSSL_cleanse(priv.buf, priv.len);
	EVP_PKEY *k = key_from(nullptr, 0, padded);
	OPENSSL_cleanse(padded, sizeof(padded));
	uint8_t sig[64];
	bool ok = k != nullptr && sign_es256(k, strbuf_str(&input), sig);
	EVP_PKEY_free(k);
	if (!ok) {
		return err_set(err, 0, "invalid VAPID private key");
	}
	if (!decode_vapid_key(public_key, &pub, err)) {
		return false;
	}
	strbuf_append_cstr(out, "vapid t=");
	strbuf_append_str(out, strbuf_str(&input));
	strbuf_append_char(out, '.');
	base64_encode(out, BASE64_URL_SAFE_NO_PAD, sig, sizeof(sig));
	strbuf_append_cstr(out, ", k=");
	base64_encode(out, BASE64_URL_SAFE_NO_PAD, pub.buf, pub.len);
	return true;
}

bool webpush_is_expired_status(int status) {
	return status == 404 || status == 410;
}
