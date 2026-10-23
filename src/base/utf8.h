// UTF-8 handling with the semantics of Rust's str, char and String, which
// soju-rs relies on. Where Rust uses Unicode properties (char::is_whitespace,
// str::to_lowercase, ...), these functions give the same answers as the Rust
// toolchain soju-rs is built with, from tables generated with it.
//
// Strings received from the network are made valid UTF-8 once, with
// utf8_lossy(); the other functions expect valid UTF-8 but never read out
// of bounds on invalid input (invalid bytes are treated as U+FFFD, one byte
// at a time).
#pragma once

#include <stddef.h>
#include <stdint.h>

#include "base/str.h"

constexpr uint32_t UTF8_REPLACEMENT = 0xFFFD;

// Decodes the code point at byte offset *i and advances *i past it.
// Returns false at the end of the string.
[[nodiscard]] bool utf8_next(struct str s, size_t *i, uint32_t *cp);
// Decodes the code point ending at byte offset *i (exclusive) and moves *i
// to its start. Returns false at the start of the string.
[[nodiscard]] bool utf8_prev(struct str s, size_t *i, uint32_t *cp);
// Appends the UTF-8 encoding of cp (cp must be a Unicode scalar value)
void utf8_push(struct strbuf *out, uint32_t cp);
// The number of code points
[[nodiscard]] size_t utf8_count(struct str s);

// Like std::str::from_utf8
[[nodiscard]] bool utf8_valid(struct str s);
// Appends s with invalid sequences replaced like String::from_utf8_lossy
// (one U+FFFD per maximal invalid subpart)
void utf8_lossy(struct strbuf *out, struct str s);
// str::is_char_boundary
[[nodiscard]] bool utf8_is_char_boundary(struct str s, size_t i);
// str::floor_char_boundary: the largest char boundary <= i
[[nodiscard]] size_t utf8_floor_char_boundary(struct str s, size_t i);

// char::is_whitespace, is_alphabetic, is_numeric, is_alphanumeric,
// is_control, is_uppercase, is_lowercase
[[nodiscard]] bool utf8_is_whitespace(uint32_t cp);
[[nodiscard]] bool utf8_is_alphabetic(uint32_t cp);
[[nodiscard]] bool utf8_is_numeric(uint32_t cp);
[[nodiscard]] bool utf8_is_alphanumeric(uint32_t cp);
[[nodiscard]] bool utf8_is_control(uint32_t cp);

// str::trim, trim_start, trim_end (Unicode whitespace)
[[nodiscard]] struct str utf8_trim(struct str s);
[[nodiscard]] struct str utf8_trim_start(struct str s);
[[nodiscard]] struct str utf8_trim_end(struct str s);
// Iterates like str::split_whitespace:
//   struct str rest = s, word;
//   while (utf8_next_word(&rest, &word)) { ... }
[[nodiscard]] bool utf8_next_word(struct str *rest, struct str *word);

// Appends s uppercased like str::to_uppercase (full case mapping: "ß"
// becomes "SS")
void utf8_to_upper(struct strbuf *out, struct str s);
// Appends s lowercased like str::to_lowercase (including the final sigma
// rule)
void utf8_to_lower(struct strbuf *out, struct str s);
