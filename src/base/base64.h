// Base64 with the semantics of the base64 crate (0.23) soju-rs uses: the
// STANDARD engine (padded, canonical padding required) and URL_SAFE_NO_PAD
// (no padding allowed). Decoding rejects non-zero trailing bits, and errors
// carry the crate's messages ("Invalid symbol 33, offset 0.", ...).
#pragma once

#include <stddef.h>
#include <stdint.h>

#include "base/err.h"
#include "base/str.h"

enum base64_variant {
	BASE64_STANDARD,
	BASE64_URL_SAFE_NO_PAD,
};

void base64_encode(struct strbuf *out, enum base64_variant v, const void *data, size_t len);
[[nodiscard]] char *base64_encode_cstr(enum base64_variant v, const void *data, size_t len);
// Appends the decoded bytes to out (a strbuf used as a byte buffer: it may
// contain NUL bytes). On error, out is left as it was.
[[nodiscard]] bool base64_decode(struct strbuf *out, enum base64_variant v, struct str s,
				 struct err **err);
