// Bump allocator: many small allocations freed together (PLAN.md 2.1 rule
// 7). Used for parsed IRC messages, whose strings all die with the message.
// Pointers into an arena are invalid after arena_free(); keeping one is a
// use-after-free that ASan reports, because chunks are real heap blocks.
#pragma once

#include <stddef.h>

#include "base/str.h"

struct arena_chunk;

struct arena {
	struct arena_chunk *head;
	size_t used; // bytes used in head
};

#define ARENA_INIT ((struct arena){0})

// Memory aligned for any type; n may be 0
[[nodiscard]] void *arena_alloc(struct arena *a, size_t n);
[[nodiscard]] char *arena_strndup(struct arena *a, const char *s, size_t n);
[[nodiscard]] char *arena_strdup(struct arena *a, const char *s);
[[nodiscard]] char *arena_str(struct arena *a, struct str s);
// Frees every allocation, leaving an empty arena
void arena_free(struct arena *a);
