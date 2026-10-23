// Borrowed string slices (struct str) and owned growable strings
// (struct strbuf), PLAN.md 2.1 rule 6.
//
// A struct str points into memory owned by someone else and is not
// NUL-terminated in general. A struct strbuf owns its buffer, which is
// always NUL-terminated once anything was appended; strbuf_cstr() never
// returns nullptr.
#pragma once

#include <stdarg.h>
#include <stddef.h>
#include <string.h>

struct str {
	const char *p;
	size_t len;
};

#define STR_LIT(s) ((struct str){.p = (s), .len = sizeof(s) - 1})
#define STR_EMPTY ((struct str){.p = "", .len = 0})
// printf("%.*s", STR_FMT(s))
#define STR_FMT(s) (int)(s).len, (s).p

static inline struct str str_from(const char *s) {
	return (struct str){.p = s, .len = strlen(s)};
}

static inline struct str str_new(const char *p, size_t len) {
	return (struct str){.p = p, .len = len};
}

[[nodiscard]] bool str_eq(struct str a, struct str b);
[[nodiscard]] bool str_eq_cstr(struct str a, const char *b);
[[nodiscard]] bool str_eq_ignore_ascii_case(struct str a, struct str b);
[[nodiscard]] bool str_starts_with(struct str s, struct str prefix);
[[nodiscard]] bool str_ends_with(struct str s, struct str suffix);
// Byte index of the first c, or -1
[[nodiscard]] ptrdiff_t str_index(struct str s, char c);
// Byte index of the last c, or -1
[[nodiscard]] ptrdiff_t str_rindex(struct str s, char c);
// Byte index of the first occurrence of needle, or -1
[[nodiscard]] ptrdiff_t str_find(struct str s, struct str needle);
[[nodiscard]] bool str_contains(struct str s, char c);
// s[start:end], clamped to the string
[[nodiscard]] struct str str_slice(struct str s, size_t start, size_t end);
// Splits at the first sep: before and after it. Returns false (and leaves
// before/after untouched) if sep isn't found.
[[nodiscard]] bool str_split_once(struct str s, char sep, struct str *before, struct str *after);
// Like str_split_once with a multi-byte separator
[[nodiscard]] bool str_split_once_str(struct str s, struct str sep, struct str *before,
				      struct str *after);
// Iterates over the fields of s separated by sep, like Rust's split():
//   struct str rest = s, field;
//   while (str_next_field(&rest, ';', &field)) { ... }
// An empty s yields one empty field; use str_next_field_init semantics:
// the iterator stops after the last field, marked by rest.p == nullptr.
[[nodiscard]] bool str_next_field(struct str *rest, char sep, struct str *field);
// Removes ASCII whitespace (space, \t, \n, \v, \f, \r) at both ends, like
// Rust's trim_ascii(). Rust's trim() removes Unicode whitespace: use
// utf8_trim() (base/utf8.h) for that.
[[nodiscard]] struct str str_trim_ascii(struct str s);
[[nodiscard]] struct str str_trim_ascii_start(struct str s);
[[nodiscard]] struct str str_trim_ascii_end(struct str s);
// Owned NUL-terminated copy
[[nodiscard]] char *str_dup(struct str s);
// Three-way byte comparison, like memcmp then shorter-first
[[nodiscard]] int str_cmp(struct str a, struct str b);
// Parses a decimal integer like Rust's i64::from_str: optional '+' or '-',
// then one or more digits, nothing else. Returns false on overflow or
// invalid input.
[[nodiscard]] bool str_parse_i64(struct str s, long long *out);
[[nodiscard]] bool str_parse_u64(struct str s, unsigned long long *out);

static inline bool cstr_eq(const char *a, const char *b) {
	return strcmp(a, b) == 0;
}

struct strbuf {
	char *buf;
	size_t len;
	size_t cap;
};

#define STRBUF_INIT ((struct strbuf){0})

void strbuf_reserve(struct strbuf *sb, size_t extra);
void strbuf_append(struct strbuf *sb, const char *p, size_t n);
void strbuf_append_str(struct strbuf *sb, struct str s);
void strbuf_append_cstr(struct strbuf *sb, const char *s);
void strbuf_append_char(struct strbuf *sb, char c);
[[gnu::format(printf, 2, 3)]] void strbuf_printf(struct strbuf *sb, const char *fmt, ...);
[[gnu::format(printf, 2, 0)]] void strbuf_vprintf(struct strbuf *sb, const char *fmt, va_list ap);
// Shortens the string to n bytes (n <= len)
void strbuf_truncate(struct strbuf *sb, size_t n);
// Empties the string, keeping the buffer
void strbuf_reset(struct strbuf *sb);
// The NUL-terminated contents, valid until the next modification
[[nodiscard]] const char *strbuf_cstr(const struct strbuf *sb);
[[nodiscard]] struct str strbuf_str(const struct strbuf *sb);
// Returns the buffer (owned by the caller, never nullptr) and resets sb
[[nodiscard]] char *strbuf_take(struct strbuf *sb);
void strbuf_free(struct strbuf *sb);

void strbuf_cleanup_impl(struct strbuf *sb);
#define AUTO_STRBUF [[gnu::cleanup(strbuf_cleanup_impl)]]
