#include "base/arena.h"

#include <stdalign.h>
#include <string.h>

#include "base/alloc.h"

static constexpr size_t CHUNK_SIZE = 512;
static constexpr size_t ALIGN = alignof(max_align_t);

struct arena_chunk {
	struct arena_chunk *next;
	size_t size;
	alignas(max_align_t) unsigned char data[];
};

void *arena_alloc(struct arena *a, size_t n) {
	n = size_add(n, ALIGN - 1) & ~(ALIGN - 1);
	if (n == 0) {
		n = ALIGN;
	}
	if (a->head == nullptr || a->head->size - a->used < n) {
		size_t size = n > CHUNK_SIZE ? n : CHUNK_SIZE;
		struct arena_chunk *c = xmalloc(size_add(sizeof(*c), size));
		c->size = size;
		if (a->head != nullptr && n > CHUNK_SIZE) {
			// A large block: keep using the current chunk's free space
			c->next = a->head->next;
			a->head->next = c;
			return c->data;
		}
		c->next = a->head;
		a->head = c;
		a->used = 0;
	}
	void *p = a->head->data + a->used;
	a->used += n;
	return p;
}

char *arena_strndup(struct arena *a, const char *s, size_t n) {
	char *p = arena_alloc(a, size_add(n, 1));
	if (n != 0) {
		memcpy(p, s, n);
	}
	p[n] = '\0';
	return p;
}

char *arena_strdup(struct arena *a, const char *s) {
	return arena_strndup(a, s, strlen(s));
}

char *arena_str(struct arena *a, struct str s) {
	return arena_strndup(a, s.p, s.len);
}

void arena_free(struct arena *a) {
	struct arena_chunk *c = a->head;
	while (c != nullptr) {
		struct arena_chunk *next = c->next;
		xfree(c);
		c = next;
	}
	*a = ARENA_INIT;
}
