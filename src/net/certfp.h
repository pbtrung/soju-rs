// Self-signed client certificate generation for SASL EXTERNAL (soju-rs
// certfp.rs)
#pragma once

#include "base/err.h"
#include "base/str.h"

// Generates a private key and a self-signed certificate: the PKCS#8 key
// and the certificate, both DER, are appended to key and cert. key_type is
// "rsa", "ecdsa" (NIST P-521) or "ed25519"; bits is the RSA key size
// (2048, 3072, 4096 or 8192).
[[nodiscard]] bool generate_certfp(const char *key_type, unsigned bits, struct strbuf *key,
				   struct strbuf *cert, struct err **err);
