// String-keyed hash map that iterates in insertion order (like Rust's
// IndexMap with shift_remove). Keys are copied; values are void pointers
// owned by the map when a free_value function is given.
//
// Keys come from the network, so hashing uses SipHash with a per-process
// random key.
//
//   struct strmap m; strmap_init(&m, free_thing);
//   strmap_set(&m, "a", thing);
//   const char *k; void *v;
//   for (size_t it = 0; strmap_next(&m, &it, &k, &v);) { ... }
//   strmap_free(&m);
#pragma once

#include <stddef.h>
#include <stdint.h>

struct strmap_entry {
	char *key; // nullptr: removed
	void *value;
	uint64_t hash;
};

struct strmap {
	struct strmap_entry *entries;
	size_t n_entries; // including removed ones
	size_t cap_entries;
	size_t live;
	size_t *index; // 0: empty, SIZE_MAX: removed, else entry index + 1
	size_t index_cap; // a power of two, or 0
	void (*free_value)(void *value);
};

void strmap_init(struct strmap *m, void (*free_value)(void *value));
void strmap_free(struct strmap *m);
// Removes everything, keeping the map usable
void strmap_clear(struct strmap *m);

[[nodiscard]] size_t strmap_len(const struct strmap *m);
[[nodiscard]] bool strmap_has(const struct strmap *m, const char *key);
// The value, or nullptr if missing (store non-null values to tell apart)
[[nodiscard]] void *strmap_get(const struct strmap *m, const char *key);
// Pointer to the stored value slot, or nullptr if missing
[[nodiscard]] void **strmap_get_slot(const struct strmap *m, const char *key);
// Inserts or replaces (the old value is freed); a replaced key keeps its
// position
void strmap_set(struct strmap *m, const char *key, void *value);
// Removes the key and frees its value; returns false if it was missing
bool strmap_remove(struct strmap *m, const char *key);
// Removes the key and returns its value without freeing it (nullptr if
// missing)
[[nodiscard]] void *strmap_take(struct strmap *m, const char *key);
// Removes the key like Rust's IndexMap::swap_remove: the last entry takes
// its position (O(1), but changes the order). Returns the value without
// freeing it (nullptr if missing).
[[nodiscard]] void *strmap_swap_take(struct strmap *m, const char *key);
// swap_take, then frees the value; returns false if the key was missing
bool strmap_swap_remove(struct strmap *m, const char *key);
// The number of entries the map can hold without growing
[[nodiscard]] size_t strmap_capacity(const struct strmap *m);
// Releases unused memory
void strmap_shrink_to_fit(struct strmap *m);
// Iterates in insertion order; *it starts at 0. Entries may be removed
// while iterating, but not added.
[[nodiscard]] bool strmap_next(const struct strmap *m, size_t *it, const char **key, void **value);

// Hashing used by the map, exposed for other hash tables
[[nodiscard]] uint64_t hash_bytes(const void *p, size_t n);
