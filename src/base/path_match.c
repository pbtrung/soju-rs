// A port of the Rust port of Go's path.Match: patterns and names are
// matched character by character (decoded to code points first).
#include "base/path_match.h"

#include <stdint.h>

#include "base/alloc.h"
#include "base/utf8.h"
#include "base/vec.h"

struct chars {
	const uint32_t *p;
	size_t len;
};

static struct chars rest(struct chars c, size_t from) {
	return (struct chars){c.p + from, c.len - from};
}

static void scan_chunk(struct chars p, bool *star, struct chars *chunk, struct chars *after) {
	*star = false;
	while (p.len > 0 && p.p[0] == '*') {
		p = rest(p, 1);
		*star = true;
	}
	bool inrange = false;
	size_t i = 0;
	for (; i < p.len; i++) {
		uint32_t c = p.p[i];
		if (c == '\\') {
			// error check handled in match_chunk: bad pattern.
			if (i + 1 < p.len) {
				i++;
			}
		} else if (c == '[') {
			inrange = true;
		} else if (c == ']') {
			inrange = false;
		} else if (c == '*' && !inrange) {
			break;
		}
	}
	*chunk = (struct chars){p.p, i};
	*after = rest(p, i);
}

static bool get_esc(struct chars *chunk, uint32_t *r) {
	if (chunk->len == 0 || chunk->p[0] == '-' || chunk->p[0] == ']') {
		return false;
	}
	if (chunk->p[0] == '\\') {
		*chunk = rest(*chunk, 1);
		if (chunk->len == 0) {
			return false;
		}
	}
	*r = chunk->p[0];
	*chunk = rest(*chunk, 1);
	return chunk->len != 0;
}

// Returns false on a bad pattern; *ok tells whether it matched, *t is the
// remaining name
static bool match_chunk(struct chars chunk, struct chars s, struct chars *t, bool *ok) {
	// failed records whether the match has failed. After the match fails,
	// the loop continues on processing chunk, checking that the pattern is
	// well-formed but no longer reading s.
	bool failed = false;
	while (chunk.len > 0) {
		if (!failed && s.len == 0) {
			failed = true;
		}
		uint32_t c = chunk.p[0];
		if (c == '[') {
			uint32_t r = 0;
			if (!failed) {
				r = s.p[0];
				s = rest(s, 1);
			}
			chunk = rest(chunk, 1);
			bool negated = false;
			if (chunk.len > 0 && chunk.p[0] == '^') {
				negated = true;
				chunk = rest(chunk, 1);
			}
			bool matched = false;
			size_t nrange = 0;
			while (true) {
				if (chunk.len > 0 && chunk.p[0] == ']' && nrange > 0) {
					chunk = rest(chunk, 1);
					break;
				}
				uint32_t lo, hi;
				if (!get_esc(&chunk, &lo)) {
					return false;
				}
				hi = lo;
				if (chunk.p[0] == '-') {
					chunk = rest(chunk, 1);
					if (!get_esc(&chunk, &hi)) {
						return false;
					}
				}
				if (lo <= r && r <= hi) {
					matched = true;
				}
				nrange++;
			}
			if (matched == negated) {
				failed = true;
			}
		} else if (c == '?') {
			if (!failed) {
				if (s.p[0] == '/') {
					failed = true;
				}
				s = rest(s, 1);
			}
			chunk = rest(chunk, 1);
		} else if (c == '\\') {
			chunk = rest(chunk, 1);
			if (chunk.len == 0) {
				return false;
			}
			if (!failed) {
				if (chunk.p[0] != s.p[0]) {
					failed = true;
				}
				s = rest(s, 1);
			}
			chunk = rest(chunk, 1);
		} else {
			if (!failed) {
				if (c != s.p[0]) {
					failed = true;
				}
				s = rest(s, 1);
			}
			chunk = rest(chunk, 1);
		}
	}
	if (failed) {
		*t = (struct chars){nullptr, 0};
		*ok = false;
	} else {
		*t = s;
		*ok = true;
	}
	return true;
}

typedef VEC(uint32_t) u32vec;

static void decode(struct str s, u32vec *out) {
	size_t i = 0;
	uint32_t cp;
	while (utf8_next(s, &i, &cp)) {
		vec_push(out, cp);
	}
}

static bool match_chars(struct chars p, struct chars n, bool *matched) {
	while (p.len > 0) {
		bool star;
		struct chars chunk, t;
		scan_chunk(p, &star, &chunk, &p);
		if (star && chunk.len == 0) {
			// Trailing * matches rest of string unless it has a /.
			*matched = true;
			for (size_t i = 0; i < n.len; i++) {
				if (n.p[i] == '/') {
					*matched = false;
				}
			}
			return true;
		}
		// Look for match at current position.
		bool ok;
		if (!match_chunk(chunk, n, &t, &ok)) {
			return false;
		}
		// if we're the last chunk, make sure we've exhausted the name
		// otherwise we'll give a false result even if we could still match
		// using the star
		if (ok && (t.len == 0 || p.len > 0)) {
			n = t;
			continue;
		}
		bool next_pattern = false;
		if (star) {
			// Look for match skipping i+1 bytes. Cannot skip /.
			for (size_t i = 0; i < n.len && n.p[i] != '/'; i++) {
				if (!match_chunk(chunk, rest(n, i + 1), &t, &ok)) {
					return false;
				}
				if (ok) {
					// if we're the last chunk, make sure we exhausted the
					// name
					if (p.len == 0 && t.len > 0) {
						continue;
					}
					n = t;
					next_pattern = true;
					break;
				}
			}
		}
		if (next_pattern) {
			continue;
		}
		// Before returning false with no error, check that the remainder
		// of the pattern is syntactically valid.
		while (p.len > 0) {
			scan_chunk(p, &star, &chunk, &p);
			if (!match_chunk(chunk, (struct chars){nullptr, 0}, &t, &ok)) {
				return false;
			}
		}
		*matched = false;
		return true;
	}
	*matched = n.len == 0;
	return true;
}

bool path_match(struct str pattern, struct str name, bool *matched) {
	u32vec p = {0};
	u32vec n = {0};
	decode(pattern, &p);
	decode(name, &n);
	bool ok =
		match_chars((struct chars){p.data, p.len}, (struct chars){n.data, n.len}, matched);
	vec_free(&p);
	vec_free(&n);
	return ok;
}
