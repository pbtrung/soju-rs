// Unicode properties come from src/base/unicode_tables.inc, generated from
// the Rust toolchain soju-rs is built with (tools/gen-unicode-tables.sh).
#include "base/utf8.h"

struct utf8_range {
	uint32_t lo, hi;
};

struct utf8_case {
	uint32_t cp;
	uint8_t n;
	uint32_t to[3];
};

#include "base/unicode_tables.inc"

static bool in_ranges(const struct utf8_range *r, size_t n, uint32_t cp) {
	size_t lo = 0, hi = n;
	while (lo < hi) {
		size_t mid = lo + (hi - lo) / 2;
		if (cp < r[mid].lo) {
			hi = mid;
		} else if (cp > r[mid].hi) {
			lo = mid + 1;
		} else {
			return true;
		}
	}
	return false;
}

#define IN(table, cp) in_ranges((table), sizeof(table) / sizeof((table)[0]), (cp))

static const struct utf8_case *find_case(const struct utf8_case *m, size_t n, uint32_t cp) {
	size_t lo = 0, hi = n;
	while (lo < hi) {
		size_t mid = lo + (hi - lo) / 2;
		if (cp < m[mid].cp) {
			hi = mid;
		} else if (cp > m[mid].cp) {
			lo = mid + 1;
		} else {
			return &m[mid];
		}
	}
	return nullptr;
}

// The length of the valid UTF-8 sequence at s[i], or 0 if it's invalid. On
// an invalid sequence, *bad is the length of its maximal valid prefix (at
// least 1): Rust's from_utf8_lossy replaces each such prefix with one
// U+FFFD.
static size_t seq_len(struct str s, size_t i, uint32_t *cp, size_t *bad) {
	const unsigned char *p = (const unsigned char *)s.p + i;
	size_t avail = s.len - i;
	unsigned char b0 = p[0];
	*bad = 1;
	if (b0 < 0x80) {
		*cp = b0;
		return 1;
	}
	size_t n;
	unsigned char lo = 0x80, hi = 0xBF;
	uint32_t c;
	if (b0 >= 0xC2 && b0 <= 0xDF) {
		n = 2;
		c = b0 & 0x1FU;
	} else if (b0 >= 0xE0 && b0 <= 0xEF) {
		n = 3;
		c = b0 & 0x0FU;
		if (b0 == 0xE0) {
			lo = 0xA0;
		} else if (b0 == 0xED) {
			hi = 0x9F;
		}
	} else if (b0 >= 0xF0 && b0 <= 0xF4) {
		n = 4;
		c = b0 & 0x07U;
		if (b0 == 0xF0) {
			lo = 0x90;
		} else if (b0 == 0xF4) {
			hi = 0x8F;
		}
	} else {
		return 0;
	}
	for (size_t k = 1; k < n; k++) {
		if (k >= avail) {
			return 0;
		}
		unsigned char b = p[k];
		if (b < lo || b > hi) {
			return 0;
		}
		lo = 0x80;
		hi = 0xBF;
		c = (c << 6U) | (b & 0x3FU);
		*bad = k + 1;
	}
	*cp = c;
	return n;
}

bool utf8_next(struct str s, size_t *i, uint32_t *cp) {
	if (*i >= s.len) {
		return false;
	}
	size_t bad;
	size_t n = seq_len(s, *i, cp, &bad);
	if (n == 0) {
		*cp = UTF8_REPLACEMENT;
		n = 1;
	}
	*i += n;
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
	uint32_t c;
	size_t bad;
	if (seq_len(s, start, &c, &bad) == *i - start) {
		*cp = c;
		*i = start;
	} else {
		*cp = UTF8_REPLACEMENT;
		*i -= 1;
	}
	return true;
}

void utf8_push(struct strbuf *out, uint32_t cp) {
	char buf[4];
	size_t n;
	if (cp < 0x80) {
		buf[0] = (char)cp;
		n = 1;
	} else if (cp < 0x800) {
		buf[0] = (char)(0xC0U | (cp >> 6U));
		buf[1] = (char)(0x80U | (cp & 0x3FU));
		n = 2;
	} else if (cp < 0x10000) {
		buf[0] = (char)(0xE0U | (cp >> 12U));
		buf[1] = (char)(0x80U | ((cp >> 6U) & 0x3FU));
		buf[2] = (char)(0x80U | (cp & 0x3FU));
		n = 3;
	} else {
		buf[0] = (char)(0xF0U | (cp >> 18U));
		buf[1] = (char)(0x80U | ((cp >> 12U) & 0x3FU));
		buf[2] = (char)(0x80U | ((cp >> 6U) & 0x3FU));
		buf[3] = (char)(0x80U | (cp & 0x3FU));
		n = 4;
	}
	strbuf_append(out, buf, n);
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
		uint32_t cp;
		size_t bad;
		size_t n = seq_len(s, i, &cp, &bad);
		if (n == 0) {
			return false;
		}
		i += n;
	}
	return true;
}

void utf8_lossy(struct strbuf *out, struct str s) {
	strbuf_reserve(out, s.len);
	size_t i = 0;
	while (i < s.len) {
		uint32_t cp;
		size_t bad;
		size_t n = seq_len(s, i, &cp, &bad);
		if (n == 0) {
			utf8_push(out, UTF8_REPLACEMENT);
			i += bad;
		} else {
			strbuf_append(out, s.p + i, n);
			i += n;
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
	return IN(alphabetic_ranges, cp);
}

bool utf8_is_numeric(uint32_t cp) {
	return IN(numeric_ranges, cp);
}

bool utf8_is_alphanumeric(uint32_t cp) {
	return utf8_is_alphabetic(cp) || utf8_is_numeric(cp);
}

bool utf8_is_control(uint32_t cp) {
	return cp <= 0x1F || (cp >= 0x7F && cp <= 0x9F);
}

bool utf8_is_lowercase(uint32_t cp) {
	return IN(lowercase_ranges, cp);
}

bool utf8_is_uppercase(uint32_t cp) {
	return IN(uppercase_ranges, cp);
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

static void push_mapped(struct strbuf *out, const struct utf8_case *m, uint32_t cp) {
	if (m == nullptr) {
		utf8_push(out, cp);
		return;
	}
	for (size_t k = 0; k < m->n; k++) {
		utf8_push(out, m->to[k]);
	}
}

void utf8_to_upper(struct strbuf *out, struct str s) {
	strbuf_reserve(out, s.len);
	size_t i = 0;
	uint32_t cp;
	while (utf8_next(s, &i, &cp)) {
		push_mapped(out, find_case(upper_map, sizeof(upper_map) / sizeof(upper_map[0]), cp),
			    cp);
	}
}

// Skips case-ignorable characters, then reports whether the next one is
// cased (core::str's case_ignorable_then_cased)
static bool ignorable_then_cased_fwd(struct str s, size_t i) {
	uint32_t cp;
	while (utf8_next(s, &i, &cp)) {
		if (!IN(case_ignorable_ranges, cp)) {
			return IN(cased_ranges, cp);
		}
	}
	return false;
}

static bool ignorable_then_cased_back(struct str s, size_t i) {
	uint32_t cp;
	while (utf8_prev(s, &i, &cp)) {
		if (!IN(case_ignorable_ranges, cp)) {
			return IN(cased_ranges, cp);
		}
	}
	return false;
}

void utf8_to_lower(struct strbuf *out, struct str s) {
	strbuf_reserve(out, s.len);
	size_t i = 0;
	uint32_t cp;
	while (true) {
		size_t start = i;
		if (!utf8_next(s, &i, &cp)) {
			break;
		}
		if (cp == 0x3A3) {
			// Σ: final form at the end of a word (str::to_lowercase)
			bool final = ignorable_then_cased_back(s, start) &&
				     !ignorable_then_cased_fwd(s, i);
			utf8_push(out, final ? 0x3C2 : 0x3C3);
			continue;
		}
		push_mapped(out, find_case(lower_map, sizeof(lower_map) / sizeof(lower_map[0]), cp),
			    cp);
	}
}

void utf8_debug_str(struct strbuf *out, struct str s) {
	strbuf_append_char(out, '"');
	size_t i = 0;
	uint32_t cp;
	while (utf8_next(s, &i, &cp)) {
		switch (cp) {
		case '\0':
			strbuf_append_cstr(out, "\\0");
			continue;
		case '\t':
			strbuf_append_cstr(out, "\\t");
			continue;
		case '\r':
			strbuf_append_cstr(out, "\\r");
			continue;
		case '\n':
			strbuf_append_cstr(out, "\\n");
			continue;
		case '\\':
			strbuf_append_cstr(out, "\\\\");
			continue;
		case '"':
			strbuf_append_cstr(out, "\\\"");
			continue;
		default:
			break;
		}
		if (IN(debug_escape_ranges, cp)) {
			strbuf_printf(out, "\\u{%x}", (unsigned)cp);
		} else {
			utf8_push(out, cp);
		}
	}
	strbuf_append_char(out, '"');
}

char *utf8_debug_cstr(struct str s) {
	struct strbuf sb = STRBUF_INIT;
	utf8_debug_str(&sb, s);
	return strbuf_take(&sb);
}
