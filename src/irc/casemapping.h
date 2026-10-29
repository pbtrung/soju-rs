// IRC case-mappings and maps keyed by case-mapped names (soju-rs
// xirc/casemapping.rs).
#pragma once

#include <stddef.h>

#include "base/map.h"
#include "base/str.h"

enum casemapping {
	CASEMAPPING_ASCII,
	CASEMAPPING_RFC1459,
	CASEMAPPING_RFC1459_STRICT,
};

// The canonical representation of a name (a new string)
[[nodiscard]] char *casemap(enum casemapping cm, struct str name);
static inline char *casemap_cstr(enum casemapping cm, const char *name) {
	return casemap(cm, str_from(name));
}
// "ascii", "rfc1459" or "rfc1459-strict"; false for anything else
[[nodiscard]] bool parse_casemapping(struct str s, enum casemapping *out);

// A map whose keys are compared according to a case-mapping, keeping the
// original spelling of each key and insertion order (CaseMappingMap).
// Values are owned by the map when free_value isn't nullptr.
struct cmmap {
	struct strmap m; // case-mapped key -> struct cmmap_entry *
	enum casemapping cm;
	void (*free_value)(void *value);
};

void cmmap_init(struct cmmap *m, enum casemapping cm, void (*free_value)(void *value));
void cmmap_free(struct cmmap *m);
void cmmap_clear(struct cmmap *m);
[[nodiscard]] size_t cmmap_len(const struct cmmap *m);
[[nodiscard]] bool cmmap_has(const struct cmmap *m, const char *name);
// The value, or nullptr if missing
[[nodiscard]] void *cmmap_get(const struct cmmap *m, const char *name);
// Sets a value (freeing the previous one). If the key already exists, its
// original spelling is kept.
void cmmap_set(struct cmmap *m, const char *name, void *value);
// Removes a key and returns its value without freeing it (nullptr if
// missing). Like upstream's unordered maps, the order of the other keys
// may change (the last one takes its place). Memory is released once most
// of the map is empty.
[[nodiscard]] void *cmmap_take(struct cmmap *m, const char *name);
// cmmap_take, then frees the value; returns false if the key was missing
bool cmmap_del(struct cmmap *m, const char *name);
// Iterates over (original key, value) in order; *it starts at 0
[[nodiscard]] bool cmmap_next(const struct cmmap *m, size_t *it, const char **name, void **value);
// Pointer to the value slot of the entry the iterator just returned
[[nodiscard]] void **cmmap_value_slot(const struct cmmap *m, size_t it);
void cmmap_set_casemapping(struct cmmap *m, enum casemapping cm);
// For tests: the stored original spelling of a case-mapped key (nullptr
// when it equals the key), and whether the key exists
[[nodiscard]] bool cmmap_peek_spelling(const struct cmmap *m, const char *cm_key,
				       const char **spelling);
[[nodiscard]] size_t cmmap_capacity(const struct cmmap *m);
