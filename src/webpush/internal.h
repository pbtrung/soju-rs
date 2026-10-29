// The OpenSSL helpers of webpush/crypto.c, shared with the sender and
// tests (which decrypt like a user agent)
#pragma once

#include <openssl/evp.h>

#include "webpush/webpush.h"

// A new P-256 key pair
[[nodiscard]] EVP_PKEY *webpush_generate_key(void);
// Uncompressed SEC1 public key (65 bytes)
[[nodiscard]] bool webpush_public_bytes(EVP_PKEY *k, uint8_t out[65]);
[[nodiscard]] bool webpush_private_bytes(EVP_PKEY *k, uint8_t out[32]);
// A public key from SEC1 bytes, nullptr if it isn't a point of the curve
[[nodiscard]] EVP_PKEY *webpush_public_key(const uint8_t *pub, size_t len);
// The ECDH shared secret (the x coordinate)
[[nodiscard]] bool webpush_ecdh(EVP_PKEY *local, EVP_PKEY *remote, uint8_t out[32]);
// HKDF-SHA-256, extract then expand
[[nodiscard]] bool webpush_hkdf(struct str salt, struct str ikm, struct str info, uint8_t *out,
				size_t len);
// webpush_encrypt with a given salt and local key
[[nodiscard]] bool webpush_encrypt_with_key(struct str message, const char *auth,
					    const char *p256dh, const uint8_t salt[16],
					    EVP_PKEY *local, struct strbuf *out, struct err **err);
