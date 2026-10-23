// Live-object tracking (PLAN.md 2.1 rule 11). Long-lived objects (users,
// networks, connections, ...) embed a struct ref_node and register it on
// creation; at shutdown and at the end of every test, nothing may be left.
// Everything runs on the loop thread: no locking.
//
//   struct conn { struct ref_node ref; ... };
//   c = xcalloc(1, sizeof(*c)); REF_TRACK(&c->ref, "conn");
//   ...; ref_untrack(&c->ref); xfree(c);
//
// Tracking is compiled in every mode but release.
#pragma once

#include <stddef.h>
#include <stdio.h>

struct ref_node {
	struct ref_node *prev, *next;
	const char *type;
	const char *file;
	int line;
};

#ifdef SOJU_MODE_RELEASE
#define SOJU_REF_TRACKING 0
#else
#define SOJU_REF_TRACKING 1
#endif

void ref_track_at(struct ref_node *node, const char *type, const char *file, int line);
void ref_untrack(struct ref_node *node);
// The number of tracked objects, optionally of one type (nullptr: all)
size_t ref_live_count(const char *type);
// Prints every tracked object; returns how many there are
size_t ref_dump_live(FILE *f);

#define REF_TRACK(node, type) ref_track_at((node), (type), __FILE__, __LINE__)

// A reference count for objects shared between several owners (e.g. a
// connection and its in-flight write requests, rule 8).
struct refcount {
	unsigned n;
};

static inline void refcount_init(struct refcount *r) {
	r->n = 1;
}

static inline void refcount_inc(struct refcount *r) {
	r->n++;
}

// Returns true when the last reference was dropped
[[nodiscard]] bool refcount_dec(struct refcount *r);
