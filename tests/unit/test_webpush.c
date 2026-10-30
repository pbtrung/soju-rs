// Ported from soju-rs src/webpush.rs (encrypt_roundtrip, vapid,
// internal_ips, external_url, expired_status)
#include "testsupport.h"

#include <arpa/inet.h>
#include <openssl/ec.h>
#include <yyjson.h>

#include "base/alloc.h"
#include "base/base64.h"
#include "http/client.h"
#include "webpush/internal.h"

// Decrypts a message as a user agent would (RFC 8291 section 3.4)
static void decrypt(const struct strbuf *body, EVP_PKEY *ua, const uint8_t auth[16],
		    struct strbuf *plain) {
	const uint8_t *b = (const uint8_t *)body->buf;
	uint32_t rs = ((uint32_t)b[16] << 24U) | ((uint32_t)b[17] << 16U) |
		      ((uint32_t)b[18] << 8U) | b[19];
	assert_int_equal(rs, WEBPUSH_RECORD_SIZE);
	size_t idlen = b[20];
	assert_int_equal(idlen, 65);
	EVP_PKEY *as = webpush_public_key(b + 21, idlen);
	assert_non_null(as);
	uint8_t ua_public[65], shared[32];
	assert_true(webpush_public_bytes(ua, ua_public));
	assert_true(webpush_ecdh(ua, as, shared));
	EVP_PKEY_free(as);
	AUTO_STRBUF struct strbuf info = STRBUF_INIT;
	strbuf_append(&info, "WebPush: info", 14);
	strbuf_append(&info, (const char *)ua_public, 65);
	strbuf_append(&info, (const char *)b + 21, 65);
	uint8_t ikm[32], cek[16], nonce[12];
	struct str salt = str_new(body->buf, 16);
	assert_true(webpush_hkdf(str_new((const char *)auth, 16), str_new((const char *)shared, 32),
				 strbuf_str(&info), ikm, 32));
	assert_true(webpush_hkdf(salt, str_new((const char *)ikm, 32),
				 str_new("Content-Encoding: aes128gcm", 28), cek, 16));
	assert_true(webpush_hkdf(salt, str_new((const char *)ikm, 32),
				 str_new("Content-Encoding: nonce", 24), nonce, 12));
	const uint8_t *ct = b + 21 + idlen;
	size_t ct_len = body->len - 21 - idlen - 16;
	EVP_CIPHER_CTX *c = EVP_CIPHER_CTX_new();
	uint8_t *out = xmalloc(ct_len);
	int n = 0, fin = 0;
	assert_int_equal(EVP_DecryptInit_ex(c, EVP_aes_128_gcm(), nullptr, nullptr, nullptr), 1);
	assert_int_equal(EVP_CIPHER_CTX_ctrl(c, EVP_CTRL_GCM_SET_IVLEN, 12, nullptr), 1);
	assert_int_equal(EVP_DecryptInit_ex(c, nullptr, nullptr, cek, nonce), 1);
	assert_int_equal(EVP_DecryptUpdate(c, out, &n, ct, (int)ct_len), 1);
	assert_int_equal(
		EVP_CIPHER_CTX_ctrl(c, EVP_CTRL_GCM_SET_TAG, 16, (void *)(uintptr_t)(ct + ct_len)),
		1);
	assert_int_equal(EVP_DecryptFinal_ex(c, out + n, &fin), 1);
	EVP_CIPHER_CTX_free(c);
	size_t len = (size_t)n + (size_t)fin;
	while (len > 0 && out[len - 1] == 0) {
		len--;
	}
	assert_true(len > 0);
	assert_int_equal(out[len - 1], 2);
	strbuf_append(plain, (const char *)out, len - 1);
	xfree(out);
}

static void test_encrypt_roundtrip(void **state) {
	(void)state;
	EVP_PKEY *ua = webpush_generate_key();
	uint8_t auth[16];
	memset(auth, 9, sizeof(auth));
	uint8_t pub[65];
	assert_true(webpush_public_bytes(ua, pub));
	AUTOFREE char *p256dh = base64_encode_cstr(BASE64_URL_SAFE_NO_PAD, pub, sizeof(pub));
	AUTOFREE char *auth_b64 = base64_encode_cstr(BASE64_URL_SAFE_NO_PAD, auth, sizeof(auth));
	AUTO_STRBUF struct strbuf body = STRBUF_INIT, plain = STRBUF_INIT;
	assert_true(webpush_encrypt(STR_LIT("@time=x PRIVMSG a :hi"), auth_b64, p256dh, &body,
				    nullptr));
	assert_int_equal(body.len, WEBPUSH_RECORD_SIZE);
	decrypt(&body, ua, auth, &plain);
	assert_sb_eq(&plain, "@time=x PRIVMSG a :hi");
	char big[3000];
	memset(big, 'x', sizeof(big));
	strbuf_reset(&body);
	struct err *e = nullptr;
	assert_false(webpush_encrypt(str_new(big, sizeof(big)), auth_b64, p256dh, &body, &e));
	assert_string_equal(err_msg(e), "payload has exceeded the maximum length");
	err_free(e);
	// C-only: standard base64 keys work too, invalid points don't
	AUTOFREE char *std = base64_encode_cstr(BASE64_STANDARD, pub, sizeof(pub));
	strbuf_reset(&body);
	assert_true(webpush_encrypt(STR_LIT("x"), auth_b64, std, &body, nullptr));
	pub[64] ^= 1U;
	AUTOFREE char *bad = base64_encode_cstr(BASE64_URL_SAFE_NO_PAD, pub, sizeof(pub));
	e = nullptr;
	assert_false(webpush_encrypt(STR_LIT("x"), auth_b64, bad, &body, &e));
	assert_string_equal(err_msg(e),
			    "Unmarshal Error: Public key is not a valid point on the curve");
	err_free(e);
	EVP_PKEY_free(ua);
}

static void test_vapid(void **state) {
	(void)state;
	char *private_key, *public_key;
	webpush_generate_vapid_keys(&private_key, &public_key);
	AUTO_STRBUF struct strbuf pub = STRBUF_INIT;
	assert_true(base64_decode(&pub, BASE64_URL_SAFE_NO_PAD, str_from(public_key), nullptr));
	assert_int_equal(pub.len, 65);
	AUTO_STRBUF struct strbuf h = STRBUF_INIT;
	assert_true(webpush_vapid_authorization("https://push.example.org/abc", "https://soju.im",
						public_key, private_key, &h, nullptr));
	struct str rest = strbuf_str(&h), jwt, k;
	assert_true(str_starts_with(rest, STR_LIT("vapid t=")));
	rest = str_slice(rest, 8, rest.len);
	assert_true(str_split_once_str(rest, STR_LIT(", k="), &jwt, &k));
	assert_str_eq(k, public_key);
	ptrdiff_t dot1 = str_index(jwt, '.'), dot2 = str_rindex(jwt, '.');
	assert_true(dot1 > 0 && dot2 > dot1);
	AUTO_STRBUF struct strbuf claims = STRBUF_INIT, sig = STRBUF_INIT;
	assert_true(base64_decode(&claims, BASE64_URL_SAFE_NO_PAD,
				  str_slice(jwt, (size_t)dot1 + 1, (size_t)dot2), nullptr));
	yyjson_doc *doc = yyjson_read(claims.buf, claims.len, 0);
	assert_non_null(doc);
	yyjson_val *root = yyjson_doc_get_root(doc);
	assert_string_equal(yyjson_get_str(yyjson_obj_get(root, "aud")),
			    "https://push.example.org");
	assert_string_equal(yyjson_get_str(yyjson_obj_get(root, "sub")), "https://soju.im");
	assert_true(yyjson_is_int(yyjson_obj_get(root, "exp")));
	yyjson_doc_free(doc);
	// The ES256 signature verifies with the public key
	assert_true(base64_decode(&sig, BASE64_URL_SAFE_NO_PAD,
				  str_slice(jwt, (size_t)dot2 + 1, jwt.len), nullptr));
	assert_int_equal(sig.len, 64);
	ECDSA_SIG *es = ECDSA_SIG_new();
	BIGNUM *r = BN_bin2bn((const uint8_t *)sig.buf, 32, nullptr);
	BIGNUM *s = BN_bin2bn((const uint8_t *)sig.buf + 32, 32, nullptr);
	assert_int_equal(ECDSA_SIG_set0(es, r, s), 1);
	unsigned char *der = nullptr;
	int der_len = i2d_ECDSA_SIG(es, &der);
	assert_true(der_len > 0);
	EVP_PKEY *vk = webpush_public_key((const uint8_t *)pub.buf, pub.len);
	assert_non_null(vk);
	EVP_MD_CTX *md = EVP_MD_CTX_new();
	assert_int_equal(EVP_DigestVerifyInit(md, nullptr, EVP_sha256(), nullptr, vk), 1);
	assert_int_equal(
		EVP_DigestVerify(md, der, (size_t)der_len, (const uint8_t *)jwt.p, (size_t)dot2),
		1);
	EVP_MD_CTX_free(md);
	EVP_PKEY_free(vk);
	OPENSSL_free(der);
	ECDSA_SIG_free(es);
	xfree(private_key);
	xfree(public_key);
}

static bool internal(const char *ip) {
	struct sockaddr_storage ss = {0};
	struct sockaddr_in *in = (struct sockaddr_in *)&ss;
	struct sockaddr_in6 *in6 = (struct sockaddr_in6 *)&ss;
	if (inet_pton(AF_INET, ip, &in->sin_addr) == 1) {
		in->sin_family = AF_INET;
	} else {
		assert_int_equal(inet_pton(AF_INET6, ip, &in6->sin6_addr), 1);
		in6->sin6_family = AF_INET6;
	}
	return http_is_internal_ip((const struct sockaddr *)&ss);
}

static void test_internal_ips(void **state) {
	(void)state;
	static const char *const yes[] = {
		"127.0.0.1",
		"10.1.2.3",
		"192.168.0.1",
		"169.254.169.254",
		"::1",
		"fe80::1",
		"fd00::1",
		"0.0.0.0",
		"::ffff:10.0.0.1",
		// soju-rs 5d2c23a
		"0.1.2.3",
		"100.100.100.200",
		"100.64.0.1",
		"192.0.0.8",
		"198.18.0.1",
		"240.0.0.1",
		"255.255.255.255",
		"fec0::1",
		"::7f00:1",
		"64:ff9b::a9fe:a9fe",
		"64:ff9b:1::1",
		"2002:a00:1::1",
	};
	for (size_t i = 0; i < sizeof(yes) / sizeof(yes[0]); i++) {
		assert_true(internal(yes[i]));
	}
	assert_false(internal("1.1.1.1"));
	assert_false(internal("2606:4700::1111"));
	assert_false(internal("100.128.0.1"));
	assert_false(internal("64:ff9b::808:808"));
	assert_false(internal("2002:808:808::1"));
	// C-only: the rest of Rust's is_private and multicast ranges
	assert_true(internal("172.16.0.1"));
	assert_true(internal("172.31.255.255"));
	assert_false(internal("172.32.0.1"));
	assert_true(internal("224.0.0.1"));
	assert_true(internal("ff02::1"));
	assert_true(internal("::"));
}

static void test_external_url(void **state) {
	(void)state;
	// Hosts which the URL parser normalizes to internal IP addresses
	static const char *const bad[] = {
		"http://127.0.0.1/x", "http://2130706433/x",  "http://0x7f.1/x",
		"http://127.1/x",     "https://[::1]:8443/x", "http://[::ffff:127.0.0.1]/x",
		"http://10.0.0.1/x",
	};
	for (size_t i = 0; i < sizeof(bad) / sizeof(bad[0]); i++) {
		struct err *e = nullptr;
		assert_false(http_check_external_url(bad[i], &e));
		assert_string_equal(err_msg(e), HTTP_INTERNAL_IP_ERROR);
		err_free(e);
	}
	assert_true(http_check_external_url("https://push.example.org/x", nullptr));
	assert_true(http_check_external_url("https://1.1.1.1/x", nullptr));
}

static void test_expired_status(void **state) {
	(void)state;
	assert_true(webpush_is_expired_status(404));
	assert_true(webpush_is_expired_status(410));
	assert_false(webpush_is_expired_status(201));
	assert_false(webpush_is_expired_status(429));
}

T_MAIN(cmocka_unit_test(test_encrypt_roundtrip), cmocka_unit_test(test_vapid),
       cmocka_unit_test(test_internal_ips), cmocka_unit_test(test_external_url),
       cmocka_unit_test(test_expired_status))
