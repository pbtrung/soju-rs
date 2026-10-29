#include "base/str.h"

#include <limits.h>
#include <stdckdint.h>
#include <stdio.h>

#include "base/alloc.h"

bool str_eq(struct str a, struct str b) {
	return a.len == b.len && (a.len == 0 || memcmp(a.p, b.p, a.len) == 0);
}

bool str_eq_cstr(struct str a, const char *b) {
	return str_eq(a, str_from(b));
}

static char ascii_lower(char c) {
	return c >= 'A' && c <= 'Z' ? (char)(c - 'A' + 'a') : c;
}

bool str_eq_ignore_ascii_case(struct str a, struct str b) {
	if (a.len != b.len) {
		return false;
	}
	for (size_t i = 0; i < a.len; i++) {
		if (ascii_lower(a.p[i]) != ascii_lower(b.p[i])) {
			return false;
		}
	}
	return true;
}

bool str_starts_with(struct str s, struct str prefix) {
	return s.len >= prefix.len && (prefix.len == 0 || memcmp(s.p, prefix.p, prefix.len) == 0);
}

bool str_ends_with(struct str s, struct str suffix) {
	return s.len >= suffix.len &&
	       (suffix.len == 0 || memcmp(s.p + s.len - suffix.len, suffix.p, suffix.len) == 0);
}

ptrdiff_t str_index(struct str s, char c) {
	if (s.len == 0) {
		return -1;
	}
	const char *q = memchr(s.p, c, s.len);
	return q != nullptr ? q - s.p : -1;
}

ptrdiff_t str_rindex(struct str s, char c) {
	for (size_t i = s.len; i > 0; i--) {
		if (s.p[i - 1] == c) {
			return (ptrdiff_t)(i - 1);
		}
	}
	return -1;
}

ptrdiff_t str_find(struct str s, struct str needle) {
	if (needle.len == 0) {
		return 0;
	}
	if (needle.len > s.len) {
		return -1;
	}
	const void *q = memmem(s.p, s.len, needle.p, needle.len);
	return q != nullptr ? (const char *)q - s.p : -1;
}

bool str_contains(struct str s, char c) {
	return str_index(s, c) >= 0;
}

struct str str_slice(struct str s, size_t start, size_t end) {
	if (end > s.len) {
		end = s.len;
	}
	if (start > end) {
		start = end;
	}
	return str_new(s.p + start, end - start);
}

bool str_split_once(struct str s, char sep, struct str *before, struct str *after) {
	ptrdiff_t i = str_index(s, sep);
	if (i < 0) {
		return false;
	}
	*before = str_new(s.p, (size_t)i);
	*after = str_new(s.p + i + 1, s.len - (size_t)i - 1);
	return true;
}

bool str_split_once_str(struct str s, struct str sep, struct str *before, struct str *after) {
	ptrdiff_t i = str_find(s, sep);
	if (i < 0) {
		return false;
	}
	*before = str_new(s.p, (size_t)i);
	*after = str_new(s.p + i + sep.len, s.len - (size_t)i - sep.len);
	return true;
}

bool str_next_field(struct str *rest, char sep, struct str *field) {
	if (rest->p == nullptr) {
		return false;
	}
	struct str before, after;
	if (str_split_once(*rest, sep, &before, &after)) {
		*field = before;
		*rest = after;
	} else {
		*field = *rest;
		rest->p = nullptr;
		rest->len = 0;
	}
	return true;
}

static bool is_ascii_space(char c) {
	return c == ' ' || c == '\t' || c == '\n' || c == '\v' || c == '\f' || c == '\r';
}

struct str str_trim_ascii_start(struct str s) {
	while (s.len > 0 && is_ascii_space(s.p[0])) {
		s.p++;
		s.len--;
	}
	return s;
}

struct str str_trim_ascii_end(struct str s) {
	while (s.len > 0 && is_ascii_space(s.p[s.len - 1])) {
		s.len--;
	}
	return s;
}

struct str str_trim_ascii(struct str s) {
	return str_trim_ascii_end(str_trim_ascii_start(s));
}

char *str_dup(struct str s) {
	return xstrndup(s.p, s.len);
}

int str_cmp(struct str a, struct str b) {
	size_t n = a.len < b.len ? a.len : b.len;
	int r = n != 0 ? memcmp(a.p, b.p, n) : 0;
	if (r != 0) {
		return r;
	}
	return a.len < b.len ? -1 : a.len > b.len ? 1 : 0;
}

static bool parse_digits(struct str s, unsigned long long limit, unsigned long long *out) {
	if (s.len == 0) {
		return false;
	}
	unsigned long long v = 0;
	for (size_t i = 0; i < s.len; i++) {
		char c = s.p[i];
		if (c < '0' || c > '9') {
			return false;
		}
		if (ckd_mul(&v, v, 10ULL) || ckd_add(&v, v, (unsigned long long)(c - '0')) ||
		    v > limit) {
			return false;
		}
	}
	*out = v;
	return true;
}

bool str_parse_u64(struct str s, unsigned long long *out) {
	if (s.len > 0 && s.p[0] == '+') {
		s = str_slice(s, 1, s.len);
	}
	return parse_digits(s, ULLONG_MAX, out);
}

bool str_parse_i64(struct str s, long long *out) {
	bool neg = false;
	if (s.len > 0 && (s.p[0] == '+' || s.p[0] == '-')) {
		neg = s.p[0] == '-';
		s = str_slice(s, 1, s.len);
	}
	unsigned long long v;
	unsigned long long limit =
		neg ? (unsigned long long)LLONG_MAX + 1 : (unsigned long long)LLONG_MAX;
	if (!parse_digits(s, limit, &v)) {
		return false;
	}
	if (neg) {
		*out = v == (unsigned long long)LLONG_MAX + 1 ? LLONG_MIN : -(long long)v;
	} else {
		*out = (long long)v;
	}
	return true;
}

void strbuf_reserve(struct strbuf *sb, size_t extra) {
	size_t need = size_add(size_add(sb->len, extra), 1);
	if (need <= sb->cap) {
		return;
	}
	size_t cap = sb->cap != 0 ? sb->cap : 16;
	while (cap < need) {
		cap = size_mul(cap, 2);
	}
	bool fresh = sb->buf == nullptr;
	sb->buf = xrealloc(sb->buf, cap);
	sb->cap = cap;
	if (fresh) {
		sb->buf[0] = '\0';
	}
}

void strbuf_append(struct strbuf *sb, const char *p, size_t n) {
	strbuf_reserve(sb, n);
	if (n != 0) {
		memcpy(sb->buf + sb->len, p, n);
	}
	sb->len += n;
	sb->buf[sb->len] = '\0';
}

void strbuf_append_str(struct strbuf *sb, struct str s) {
	strbuf_append(sb, s.p, s.len);
}

void strbuf_append_cstr(struct strbuf *sb, const char *s) {
	strbuf_append(sb, s, strlen(s));
}

void strbuf_append_char(struct strbuf *sb, char c) {
	strbuf_append(sb, &c, 1);
}

void strbuf_vprintf(struct strbuf *sb, const char *fmt, va_list ap) {
	va_list ap2;
	va_copy(ap2, ap);
	int n = vsnprintf(nullptr, 0, fmt, ap2);
	va_end(ap2);
	if (n < 0) {
		die_oom();
	}
	strbuf_reserve(sb, (size_t)n);
	int m = vsnprintf(sb->buf + sb->len, (size_t)n + 1, fmt, ap);
	if (m != n) {
		die_oom();
	}
	sb->len += (size_t)n;
}

void strbuf_printf(struct strbuf *sb, const char *fmt, ...) {
	va_list ap;
	va_start(ap, fmt);
	strbuf_vprintf(sb, fmt, ap);
	va_end(ap);
}

void strbuf_truncate(struct strbuf *sb, size_t n) {
	if (n < sb->len) {
		sb->len = n;
		sb->buf[n] = '\0';
	}
}

void strbuf_drop_front(struct strbuf *sb, size_t n) {
	if (n == 0 || sb->buf == nullptr) {
		return;
	}
	memmove(sb->buf, sb->buf + n, sb->len - n);
	strbuf_truncate(sb, sb->len - n);
}

void strbuf_reset(struct strbuf *sb) {
	strbuf_truncate(sb, 0);
}

const char *strbuf_cstr(const struct strbuf *sb) {
	return sb->buf != nullptr ? sb->buf : "";
}

struct str strbuf_str(const struct strbuf *sb) {
	return str_new(strbuf_cstr(sb), sb->len);
}

char *strbuf_take(struct strbuf *sb) {
	char *s = sb->buf != nullptr ? sb->buf : xstrdup("");
	*sb = STRBUF_INIT;
	return s;
}

void strbuf_free(struct strbuf *sb) {
	xfree(sb->buf);
	*sb = STRBUF_INIT;
}

void strbuf_cleanup_impl(struct strbuf *sb) {
	strbuf_free(sb);
}
