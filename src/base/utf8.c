// Initial implementation on top of utf8proc. To be replaced by tables
// generated with the Rust toolchain for exact parity with soju-rs.
#include "base/utf8.h"

#include <utf8proc.h>

bool utf8_next(struct str s, size_t *i, uint32_t *cp) {
	if (*i >= s.len) {
		return false;
	}
	utf8proc_int32_t c;
	utf8proc_ssize_t n = utf8proc_iterate((const utf8proc_uint8_t *)s.p + *i,
					      (utf8proc_ssize_t)(s.len - *i), &c);
	if (n <= 0) {
		*cp = UTF8_REPLACEMENT;
		*i += 1;
	} else {
		*cp = (uint32_t)c;
		*i += (size_t)n;
	}
	return true;
}

bool utf8_prev(struct str s, size_t *i, uint32_t *cp) {
	if (*i == 0) {
		return false;
	}
	size_t start = *i - 1;
	while (start > 0 && *i - start < 4 && ((unsigned char)s.p[start] & 0xC0U) == 0x80U) {
		start--;
	}
	size_t j = start;
	uint32_t c;
	if (utf8_next(s, &j, &c) && j == *i) {
		*cp = c;
		*i = start;
	} else {
		*cp = UTF8_REPLACEMENT;
		*i -= 1;
	}
	return true;
}

void utf8_push(struct strbuf *out, uint32_t cp) {
	utf8proc_uint8_t buf[4];
	utf8proc_ssize_t n = utf8proc_encode_char((utf8proc_int32_t)cp, buf);
	strbuf_append(out, (const char *)buf, (size_t)n);
}

size_t utf8_count(struct str s) {
	size_t n = 0, i = 0;
	uint32_t cp;
	while (utf8_next(s, &i, &cp)) {
		n++;
	}
	return n;
}

bool utf8_valid(struct str s) {
	size_t i = 0;
	while (i < s.len) {
		utf8proc_int32_t c;
		utf8proc_ssize_t n = utf8proc_iterate((const utf8proc_uint8_t *)s.p + i,
						      (utf8proc_ssize_t)(s.len - i), &c);
		if (n <= 0) {
			return false;
		}
		i += (size_t)n;
	}
	return true;
}

void utf8_lossy(struct strbuf *out, struct str s) {
	size_t i = 0;
	while (i < s.len) {
		size_t start = i;
		uint32_t cp;
		(void)utf8_next(s, &i, &cp);
		if (cp == UTF8_REPLACEMENT && i - start == 1 && (unsigned char)s.p[start] != 0xEF) {
			utf8_push(out, UTF8_REPLACEMENT);
		} else {
			strbuf_append(out, s.p + start, i - start);
		}
	}
}

bool utf8_is_char_boundary(struct str s, size_t i) {
	if (i == 0 || i == s.len) {
		return true;
	}
	if (i > s.len) {
		return false;
	}
	return ((unsigned char)s.p[i] & 0xC0U) != 0x80U;
}

size_t utf8_floor_char_boundary(struct str s, size_t i) {
	if (i >= s.len) {
		return s.len;
	}
	while (i > 0 && !utf8_is_char_boundary(s, i)) {
		i--;
	}
	return i;
}

bool utf8_is_whitespace(uint32_t cp) {
	return (cp >= 0x09 && cp <= 0x0D) || cp == 0x20 || cp == 0x85 || cp == 0xA0 ||
	       cp == 0x1680 || (cp >= 0x2000 && cp <= 0x200A) || cp == 0x2028 || cp == 0x2029 ||
	       cp == 0x202F || cp == 0x205F || cp == 0x3000;
}

bool utf8_is_alphabetic(uint32_t cp) {
	utf8proc_category_t c = utf8proc_category((utf8proc_int32_t)cp);
	return c == UTF8PROC_CATEGORY_LU || c == UTF8PROC_CATEGORY_LL ||
	       c == UTF8PROC_CATEGORY_LT || c == UTF8PROC_CATEGORY_LM ||
	       c == UTF8PROC_CATEGORY_LO || c == UTF8PROC_CATEGORY_NL;
}

bool utf8_is_numeric(uint32_t cp) {
	utf8proc_category_t c = utf8proc_category((utf8proc_int32_t)cp);
	return c == UTF8PROC_CATEGORY_ND || c == UTF8PROC_CATEGORY_NL || c == UTF8PROC_CATEGORY_NO;
}

bool utf8_is_alphanumeric(uint32_t cp) {
	return utf8_is_alphabetic(cp) || utf8_is_numeric(cp);
}

bool utf8_is_control(uint32_t cp) {
	return utf8proc_category((utf8proc_int32_t)cp) == UTF8PROC_CATEGORY_CC;
}

struct str utf8_trim_start(struct str s) {
	size_t i = 0, prev = 0;
	uint32_t cp;
	while (utf8_next(s, &i, &cp) && utf8_is_whitespace(cp)) {
		prev = i;
	}
	return str_slice(s, prev, s.len);
}

struct str utf8_trim_end(struct str s) {
	size_t i = s.len, prev = s.len;
	uint32_t cp;
	while (utf8_prev(s, &i, &cp) && utf8_is_whitespace(cp)) {
		prev = i;
	}
	return str_slice(s, 0, prev);
}

struct str utf8_trim(struct str s) {
	return utf8_trim_end(utf8_trim_start(s));
}

bool utf8_next_word(struct str *rest, struct str *word) {
	struct str s = utf8_trim_start(*rest);
	if (s.len == 0) {
		*rest = s;
		return false;
	}
	size_t i = 0, end = 0;
	uint32_t cp;
	while (utf8_next(s, &i, &cp) && !utf8_is_whitespace(cp)) {
		end = i;
	}
	*word = str_slice(s, 0, end);
	*rest = str_slice(s, end, s.len);
	return true;
}

static void map_chars(struct strbuf *out, struct str s, utf8proc_int32_t (*f)(utf8proc_int32_t)) {
	strbuf_reserve(out, s.len);
	size_t i = 0;
	uint32_t cp;
	while (utf8_next(s, &i, &cp)) {
		utf8_push(out, (uint32_t)f((utf8proc_int32_t)cp));
	}
}

void utf8_to_upper(struct strbuf *out, struct str s) {
	map_chars(out, s, utf8proc_toupper);
}

void utf8_to_lower(struct strbuf *out, struct str s) {
	map_chars(out, s, utf8proc_tolower);
}
