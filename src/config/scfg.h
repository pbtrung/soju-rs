// Parser for the scfg configuration file format, compatible with
// codeberg.org/emersion/go-scfg (soju-rs scfg.rs)
#pragma once

#include <stddef.h>

#include "base/err.h"
#include "base/str.h"
#include "base/vec.h"

struct scfg_directive;

struct scfg_block {
	VEC(struct scfg_directive) v;
};

struct scfg_directive {
	char *name;
	strvec params;
	bool has_children; // false: no block; true with an empty block: "{ }"
	struct scfg_block children;
	size_t lineno;
};

[[nodiscard]] bool scfg_parse(struct str s, struct scfg_block *out, struct err **err);
void scfg_block_free(struct scfg_block *b);
// Splits a line into words (exposed for tests)
[[nodiscard]] bool scfg_split_words(struct str l, strvec *words, struct err **err);
