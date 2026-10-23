#include "base/ref.h"

#include <stdlib.h>
#include <string.h>

// Sentinel of the circular list of live objects
static struct ref_node live = {.prev = &live, .next = &live};

void ref_track_at(struct ref_node *node, const char *type, const char *file, int line) {
	node->type = type;
	node->file = file;
	node->line = line;
#if SOJU_REF_TRACKING
	node->prev = live.prev;
	node->next = &live;
	live.prev->next = node;
	live.prev = node;
#else
	node->prev = node->next = nullptr;
#endif
}

void ref_untrack(struct ref_node *node) {
#if SOJU_REF_TRACKING
	if (node->next == nullptr || node->prev == nullptr) {
		// Untracked twice, or never tracked: a lifetime bug
		fprintf(stderr, "ref_untrack: %s (%s:%d) isn't tracked\n",
			node->type != nullptr ? node->type : "?",
			node->file != nullptr ? node->file : "?", node->line);
		abort();
	}
	node->prev->next = node->next;
	node->next->prev = node->prev;
#endif
	node->prev = node->next = nullptr;
}

size_t ref_live_count(const char *type) {
	size_t n = 0;
	for (const struct ref_node *it = live.next; it != &live; it = it->next) {
		if (type == nullptr || strcmp(it->type, type) == 0) {
			n++;
		}
	}
	return n;
}

size_t ref_dump_live(FILE *f) {
	size_t n = 0;
	for (const struct ref_node *it = live.next; it != &live; it = it->next) {
		fprintf(f, "live object: %s, created at %s:%d\n", it->type, it->file, it->line);
		n++;
	}
	return n;
}

bool refcount_dec(struct refcount *r) {
	if (r->n == 0) {
		fprintf(stderr, "refcount_dec: count already zero\n");
		abort();
	}
	return --r->n == 0;
}
