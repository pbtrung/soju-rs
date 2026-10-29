#include "base/base64.h"

#include "base/alloc.h"

static const char std_alphabet[] =
	"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
static const char url_alphabet[] =
	"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

static constexpr uint8_t INVALID = 0xFF;

static const char *alphabet(enum base64_variant v) {
	return v == BASE64_STANDARD ? std_alphabet : url_alphabet;
}

void base64_encode(struct strbuf *out, enum base64_variant v, const void *data, size_t len) {
	const char *a = alphabet(v);
	const unsigned char *p = data;
	strbuf_reserve(out, size_mul(size_add(len, 2) / 3, 4));
	size_t i = 0;
	for (; i + 3 <= len; i += 3) {
		uint32_t n = ((uint32_t)p[i] << 16U) | ((uint32_t)p[i + 1] << 8U) | p[i + 2];
		char q[4] = {a[(n >> 18U) & 63U], a[(n >> 12U) & 63U], a[(n >> 6U) & 63U],
			     a[n & 63U]};
		strbuf_append(out, q, 4);
	}
	size_t rem = len - i;
	if (rem > 0) {
		uint32_t n = (uint32_t)p[i] << 16U;
		if (rem == 2) {
			n |= (uint32_t)p[i + 1] << 8U;
		}
		strbuf_append_char(out, a[(n >> 18U) & 63U]);
		strbuf_append_char(out, a[(n >> 12U) & 63U]);
		if (rem == 2) {
			strbuf_append_char(out, a[(n >> 6U) & 63U]);
		}
		if (v == BASE64_STANDARD) {
			strbuf_append(out, "==", 3 - rem);
		}
	}
}

char *base64_encode_cstr(enum base64_variant v, const void *data, size_t len) {
	struct strbuf sb = STRBUF_INIT;
	base64_encode(&sb, v, data, len);
	return strbuf_take(&sb);
}

static uint8_t sym(enum base64_variant v, unsigned char c) {
	if (c >= 'A' && c <= 'Z') {
		return (uint8_t)(c - 'A');
	}
	if (c >= 'a' && c <= 'z') {
		return (uint8_t)(c - 'a' + 26);
	}
	if (c >= '0' && c <= '9') {
		return (uint8_t)(c - '0' + 52);
	}
	if (v == BASE64_STANDARD) {
		return c == '+' ? 62 : c == '/' ? 63 : INVALID;
	}
	return c == '-' ? 62 : c == '_' ? 63 : INVALID;
}

static bool invalid_byte(struct err **err, size_t offset, unsigned char b) {
	return err_set(err, 0, "Invalid symbol %u, offset %zu.", (unsigned)b, offset);
}

// The decoder of the base64 crate's GeneralPurpose engine: all complete
// quads but the last one, then a suffix of 1 to 4 bytes holding padding.
// Errors are reported in the order the crate finds them.
bool base64_decode(struct strbuf *out, enum base64_variant v, struct str s, struct err **err) {
	const unsigned char *in = (const unsigned char *)s.p;
	size_t len = s.len;
	size_t rem = len % 4;
	if (rem == 1 && in[len - 1] != '=' && sym(v, in[len - 1]) == INVALID) {
		return invalid_byte(err, len - 1, in[len - 1]);
	}
	size_t quads_end = len - rem;
	if (rem == 0 && quads_end >= 4) {
		quads_end -= 4;
	}
	size_t start_len = out->len;
	strbuf_reserve(out, size_mul(len / 4 + 1, 3));
	for (size_t i = 0; i < quads_end; i += 4) {
		uint32_t n = 0;
		for (size_t k = 0; k < 4; k++) {
			uint8_t m = sym(v, in[i + k]);
			if (m == INVALID) {
				strbuf_truncate(out, start_len);
				return invalid_byte(err, i + k, in[i + k]);
			}
			n = (n << 6U) | m;
		}
		char b[3] = {(char)(n >> 16U), (char)(n >> 8U), (char)n};
		strbuf_append(out, b, 3);
	}

	size_t morsels_n = 0, padding = 0, first_padding = 0;
	uint8_t morsels[4] = {0};
	unsigned char last_symbol = 0;
	uint8_t last_value = 0;
	for (size_t k = 0; quads_end + k < len; k++) {
		unsigned char b = in[quads_end + k];
		if (b == '=') {
			if (k < 2) {
				strbuf_truncate(out, start_len);
				return invalid_byte(err, quads_end + k, b);
			}
			if (padding == 0) {
				first_padding = k;
			}
			padding++;
			continue;
		}
		if (padding > 0) {
			strbuf_truncate(out, start_len);
			return invalid_byte(err, quads_end + first_padding, '=');
		}
		last_symbol = b;
		uint8_t m = sym(v, b);
		last_value = m;
		if (m == INVALID) {
			strbuf_truncate(out, start_len);
			return invalid_byte(err, quads_end + k, b);
		}
		morsels[morsels_n++] = m;
	}
	if (len > 0 && morsels_n < 2) {
		strbuf_truncate(out, start_len);
		return err_set(err, 0, "Invalid input length: %zu", quads_end + morsels_n);
	}
	if (v == BASE64_STANDARD ? (padding + morsels_n) % 4 != 0 : padding > 0) {
		strbuf_truncate(out, start_len);
		return err_set(err, 0, "Invalid padding");
	}
	size_t nbytes = morsels_n * 6 / 8;
	uint32_t num = ((uint32_t)morsels[0] << 26U) | ((uint32_t)morsels[1] << 20U) |
		       ((uint32_t)morsels[2] << 14U) | ((uint32_t)morsels[3] << 8U);
	uint32_t mask = nbytes >= 4 ? 0 : UINT32_MAX >> (nbytes * 8);
	if ((num & mask) != 0) {
		strbuf_truncate(out, start_len);
		// {:#4x} ('{}') ... {:#010b}
		char bits[9];
		for (unsigned k = 0; k < 8; k++) {
			bits[k] = (char)((((unsigned)last_value >> (7U - k)) & 1U) != 0 ? '1' : '0');
		}
		bits[8] = '\0';
		return err_set(err, 0,
			       "Invalid last symbol %#4x ('%c') at offset %zu, decoded as 0b%s.",
			       (unsigned)last_symbol, last_symbol, quads_end + morsels_n - 1, bits);
	}
	for (size_t k = 0; k < nbytes; k++) {
		strbuf_append_char(out, (char)(num >> 24U));
		num <<= 8U;
	}
	return true;
}
