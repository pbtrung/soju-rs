#include "config/scfg.h"

#include "base/alloc.h"
#include "base/utf8.h"

// Limits the max block nesting depth to prevent stack overflows
static constexpr size_t MAX_NESTING_DEPTH = 1000;

struct decoder {
	struct str rest; // remaining input, nullptr p once all lines are read
	bool done;
	size_t lineno;
	size_t depth;
};

// str::lines(): lines end with "\n" or "\r\n"; a final empty line isn't
// returned
static bool next_line(struct decoder *d, struct str *line) {
	if (d->done || d->rest.len == 0) {
		return false;
	}
	ptrdiff_t i = str_index(d->rest, '\n');
	struct str l;
	if (i < 0) {
		l = d->rest;
		d->done = true;
	} else {
		l = str_slice(d->rest, 0, (size_t)i);
		d->rest = str_slice(d->rest, (size_t)i + 1, d->rest.len);
		if (l.len > 0 && l.p[l.len - 1] == '\r') {
			l.len--;
		}
	}
	*line = l;
	return true;
}

static void directive_free(struct scfg_directive *d) {
	xfree(d->name);
	strvec_free(&d->params);
	scfg_block_free(&d->children);
}

void scfg_block_free(struct scfg_block *b) {
	vec_free_with(&b->v, directive_free);
}

bool scfg_split_words(struct str l, strvec *words, struct err **err) {
	strvec out = {0};
	AUTO_STRBUF struct strbuf sb = STRBUF_INIT;
	bool escape = false, want_wsp = false;
	uint32_t quote = 0;
	size_t i = 0;
	uint32_t ch;
	while (utf8_next(l, &i, &ch)) {
		if (escape) {
			utf8_push(&sb, ch);
			escape = false;
		} else if (want_wsp && ch != ' ' && ch != '\t') {
			strvec_free(&out);
			return err_set(err, 0, "atom not allowed after quoted string");
		} else if (ch == '\\') {
			escape = true;
		} else if (quote != 0 && quote == ch) {
			quote = 0;
			want_wsp = true;
			if (sb.len == 0) {
				strvec_push(&out, "");
			}
		} else if (quote == 0 && out.len == 0 && sb.len == 0 && ch == '#') {
			strvec_free(&out);
			return true;
		} else if (quote == 0 && (ch == '\'' || ch == '"')) {
			if (sb.len > 0) {
				strvec_free(&out);
				return err_set(err, 0, "quoted string not allowed after atom");
			}
			quote = ch;
		} else if (quote == 0 && (ch == ' ' || ch == '\t')) {
			if (sb.len > 0) {
				strvec_push(&out, strbuf_cstr(&sb));
				strbuf_reset(&sb);
			}
			want_wsp = false;
		} else {
			utf8_push(&sb, ch);
		}
	}
	if (quote != 0) {
		strvec_free(&out);
		return err_set(err, 0, "unterminated quoted string");
	}
	if (sb.len > 0) {
		strvec_push(&out, strbuf_cstr(&sb));
	}
	for (size_t k = 0; k < out.len; k++) {
		strvec_push_take(words, out.data[k]);
	}
	vec_free(&out);
	return true;
}

static bool read_block(struct decoder *d, struct scfg_block *block, bool *closing,
		       struct err **err);

static bool read_block_inner(struct decoder *d, struct scfg_block *block, bool *closing,
			     struct err **err) {
	*closing = false;
	if (d->depth >= MAX_NESTING_DEPTH) {
		return err_set(err, 0, "exceeded max block depth");
	}
	struct str l;
	while (next_line(d, &l)) {
		d->lineno++;
		strvec words = {0};
		struct err *e = nullptr;
		if (!scfg_split_words(l, &words, &e)) {
			err_set(err, 0, "line %zu: %s", d->lineno, err_msg(e));
			err_free(e);
			return false;
		}
		if (words.len == 0) {
			continue;
		}
		if (words.len == 1 && l.len > 0 && l.p[l.len - 1] == '}') {
			strvec_free(&words);
			*closing = true;
			return true;
		}

		struct scfg_directive dir = {0};
		if (cstr_eq(words.data[words.len - 1], "{") && l.len > 0 && l.p[l.len - 1] == '{') {
			xfree(words.data[--words.len]);
			if (words.len == 0) {
				dir.name = xstrdup("");
			} else {
				dir.name = words.data[0];
				vec_remove(&words, 0);
			}
			dir.params = words;
			size_t start = d->lineno;
			bool child_closing;
			dir.has_children = true;
			if (!read_block(d, &dir.children, &child_closing, err)) {
				directive_free(&dir);
				return false;
			}
			if (!child_closing) {
				directive_free(&dir);
				return err_set(err, 0, "line %zu: unterminated block", start);
			}
		} else {
			dir.name = words.data[0];
			vec_remove(&words, 0);
			dir.params = words;
		}
		dir.lineno = d->lineno;
		vec_push(&block->v, dir);
	}
	return true;
}

static bool read_block(struct decoder *d, struct scfg_block *block, bool *closing,
		       struct err **err) {
	d->depth++;
	bool ok = read_block_inner(d, block, closing, err);
	d->depth--;
	return ok;
}

bool scfg_parse(struct str s, struct scfg_block *out, struct err **err) {
	struct decoder d = {.rest = s};
	struct scfg_block block = {0};
	bool closing;
	if (!read_block(&d, &block, &closing, err)) {
		scfg_block_free(&block);
		return false;
	}
	if (closing) {
		scfg_block_free(&block);
		return err_set(err, 0, "line %zu: unexpected '}'", d.lineno);
	}
	*out = block;
	return true;
}
