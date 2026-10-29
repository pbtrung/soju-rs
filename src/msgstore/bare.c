#include "msgstore/bare.h"

#include "base/utf8.h"

void bare_write_uint(struct strbuf *w, uint64_t v) {
	while (v >= 0x80) {
		strbuf_append_char(w, (char)((v & 0xFFU) | 0x80U));
		v >>= 7U;
	}
	strbuf_append_char(w, (char)v);
}

void bare_write_int(struct strbuf *w, int64_t v) {
	// Zigzag encoding, in unsigned arithmetic
	uint64_t u = (uint64_t)v;
	bare_write_uint(w, (u << 1U) ^ (0 - (u >> 63U)));
}

void bare_write_string(struct strbuf *w, struct str s) {
	bare_write_uint(w, s.len);
	strbuf_append_str(w, s);
}

bool bare_read_uint(struct str *r, uint64_t *out, struct err **err) {
	uint64_t v = 0;
	for (unsigned i = 0; i < 10; i++) {
		if (r->len == 0) {
			return err_set(err, 0, "unexpected EOF");
		}
		unsigned char b = (unsigned char)r->p[0];
		*r = str_slice(*r, 1, r->len);
		if (i == 9 && b > 1) {
			return err_set(err, 0, "varint overflows a 64-bit integer");
		}
		v |= (uint64_t)(b & 0x7FU) << (7U * i);
		if (b < 0x80) {
			*out = v;
			return true;
		}
	}
	return err_set(err, 0, "varint overflows a 64-bit integer");
}

bool bare_read_int(struct str *r, int64_t *out, struct err **err) {
	uint64_t u = 0;
	if (!bare_read_uint(r, &u, err)) {
		return false;
	}
	*out = (int64_t)((u >> 1U) ^ (0 - (u & 1U)));
	return true;
}

bool bare_read_string(struct str *r, struct strbuf *out, struct err **err) {
	uint64_t n = 0;
	if (!bare_read_uint(r, &n, err)) {
		return false;
	}
	if (n > r->len) {
		return err_set(err, 0, "unexpected EOF");
	}
	utf8_lossy(out, str_slice(*r, 0, (size_t)n));
	*r = str_slice(*r, (size_t)n, r->len);
	return true;
}
