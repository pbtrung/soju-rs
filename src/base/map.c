#include "base/map.h"

#include <stdlib.h>
#include <pthread.h>
#include <string.h>
#include <sys/random.h>

#include "base/alloc.h"

static constexpr size_t SLOT_EMPTY = 0;
static constexpr size_t SLOT_REMOVED = SIZE_MAX;

// SipHash-1-3 (the variant Rust's HashMap uses)

static uint64_t sip_key[2];
static pthread_once_t sip_once = PTHREAD_ONCE_INIT;

static uint64_t rotl(uint64_t x, unsigned b) {
	return (x << b) | (x >> (64 - b));
}

#define SIPROUND                   \
	do {                       \
		v0 += v1;          \
		v1 = rotl(v1, 13); \
		v1 ^= v0;          \
		v0 = rotl(v0, 32); \
		v2 += v3;          \
		v3 = rotl(v3, 16); \
		v3 ^= v2;          \
		v0 += v3;          \
		v3 = rotl(v3, 21); \
		v3 ^= v0;          \
		v2 += v1;          \
		v1 = rotl(v1, 17); \
		v1 ^= v2;          \
		v2 = rotl(v2, 32); \
	} while (0)

static void init_sip_key(void) {
	unsigned char buf[16];
	size_t got = 0;
	while (got < sizeof(buf)) {
		ssize_t n = getrandom(buf + got, sizeof(buf) - got, 0);
		if (n <= 0) {
			abort();
		}
		got += (size_t)n;
	}
	memcpy(sip_key, buf, sizeof(buf));
}

uint64_t hash_bytes(const void *p, size_t n) {
	pthread_once(&sip_once, init_sip_key);
	const unsigned char *in = p;
	uint64_t v0 = 0x736f6d6570736575ULL ^ sip_key[0];
	uint64_t v1 = 0x646f72616e646f6dULL ^ sip_key[1];
	uint64_t v2 = 0x6c7967656e657261ULL ^ sip_key[0];
	uint64_t v3 = 0x7465646279746573ULL ^ sip_key[1];
	uint64_t b = (uint64_t)n << 56U;
	size_t i = 0;
	for (; i + 8 <= n; i += 8) {
		uint64_t m;
		memcpy(&m, in + i, 8);
		v3 ^= m;
		SIPROUND;
		v0 ^= m;
	}
	for (size_t j = 0; i + j < n; j++) {
		b |= (uint64_t)in[i + j] << (8 * j);
	}
	v3 ^= b;
	SIPROUND;
	v0 ^= b;
	v2 ^= 0xffU;
	SIPROUND;
	SIPROUND;
	SIPROUND;
	return v0 ^ v1 ^ v2 ^ v3;
}

void strmap_init(struct strmap *m, void (*free_value)(void *value)) {
	*m = (struct strmap){.free_value = free_value};
}

static void free_entries(struct strmap *m) {
	for (size_t i = 0; i < m->n_entries; i++) {
		struct strmap_entry *e = &m->entries[i];
		if (e->key == nullptr) {
			continue;
		}
		xfree(e->key);
		if (m->free_value != nullptr) {
			m->free_value(e->value);
		}
	}
}

void strmap_free(struct strmap *m) {
	free_entries(m);
	xfree(m->entries);
	xfree(m->index);
	*m = (struct strmap){.free_value = m->free_value};
}

void strmap_clear(struct strmap *m) {
	strmap_free(m);
}

size_t strmap_len(const struct strmap *m) {
	return m->live;
}

// Returns the index slot holding key, or the slot where it would go (first
// removed or empty slot) when missing; *found tells which.
static size_t find_slot(const struct strmap *m, const char *key, uint64_t hash, bool *found) {
	size_t mask = m->index_cap - 1;
	size_t i = (size_t)hash & mask;
	size_t first_removed = SIZE_MAX;
	for (size_t probe = 0; probe < m->index_cap; probe++) {
		size_t slot = m->index[i];
		if (slot == SLOT_EMPTY) {
			*found = false;
			return first_removed != SIZE_MAX ? first_removed : i;
		}
		if (slot == SLOT_REMOVED) {
			if (first_removed == SIZE_MAX) {
				first_removed = i;
			}
		} else {
			const struct strmap_entry *e = &m->entries[slot - 1];
			if (e->hash == hash && strcmp(e->key, key) == 0) {
				*found = true;
				return i;
			}
		}
		i = (i + 1) & mask;
	}
	*found = false;
	return first_removed;
}

// Rebuilds the index (and drops removed entries) with room for n entries
static void rehash(struct strmap *m, size_t n) {
	size_t j = 0;
	for (size_t i = 0; i < m->n_entries; i++) {
		if (m->entries[i].key != nullptr) {
			m->entries[j++] = m->entries[i];
		}
	}
	m->n_entries = j;

	size_t cap = 8;
	while (cap < size_mul(n, 2)) {
		cap = size_mul(cap, 2);
	}
	xfree(m->index);
	m->index = xcalloc(cap, sizeof(*m->index));
	m->index_cap = cap;
	for (size_t i = 0; i < m->n_entries; i++) {
		size_t k = (size_t)m->entries[i].hash & (cap - 1);
		while (m->index[k] != SLOT_EMPTY) {
			k = (k + 1) & (cap - 1);
		}
		m->index[k] = i + 1;
	}
}

static size_t lookup(const struct strmap *m, const char *key) {
	if (m->live == 0) {
		return SIZE_MAX;
	}
	bool found;
	size_t slot = find_slot(m, key, hash_bytes(key, strlen(key)), &found);
	return found ? slot : SIZE_MAX;
}

bool strmap_has(const struct strmap *m, const char *key) {
	return lookup(m, key) != SIZE_MAX;
}

void **strmap_get_slot(const struct strmap *m, const char *key) {
	size_t slot = lookup(m, key);
	if (slot == SIZE_MAX) {
		return nullptr;
	}
	return &m->entries[m->index[slot] - 1].value;
}

void *strmap_get(const struct strmap *m, const char *key) {
	void **v = strmap_get_slot(m, key);
	return v != nullptr ? *v : nullptr;
}

void strmap_set(struct strmap *m, const char *key, void *value) {
	uint64_t hash = hash_bytes(key, strlen(key));
	// Keep the index at most half full, counting removed entries
	if (size_mul(size_add(m->n_entries, 1), 2) > m->index_cap) {
		rehash(m, size_add(m->live, 1));
	}
	bool found;
	size_t slot = find_slot(m, key, hash, &found);
	if (found) {
		struct strmap_entry *e = &m->entries[m->index[slot] - 1];
		if (m->free_value != nullptr && e->value != value) {
			m->free_value(e->value);
		}
		e->value = value;
		return;
	}
	if (m->n_entries == m->cap_entries) {
		m->cap_entries = m->cap_entries != 0 ? size_mul(m->cap_entries, 2) : 4;
		m->entries = xreallocarray(m->entries, m->cap_entries, sizeof(*m->entries));
	}
	m->entries[m->n_entries] = (struct strmap_entry){
		.key = xstrdup(key),
		.value = value,
		.hash = hash,
	};
	m->index[slot] = ++m->n_entries;
	m->live++;
}

void *strmap_take(struct strmap *m, const char *key) {
	size_t slot = lookup(m, key);
	if (slot == SIZE_MAX) {
		return nullptr;
	}
	struct strmap_entry *e = &m->entries[m->index[slot] - 1];
	void *value = e->value;
	xfree(e->key);
	e->key = nullptr;
	e->value = nullptr;
	m->index[slot] = SLOT_REMOVED;
	m->live--;
	return value;
}

bool strmap_remove(struct strmap *m, const char *key) {
	if (!strmap_has(m, key)) {
		return false;
	}
	void *value = strmap_take(m, key);
	if (m->free_value != nullptr) {
		m->free_value(value);
	}
	return true;
}

bool strmap_next(const struct strmap *m, size_t *it, const char **key, void **value) {
	while (*it < m->n_entries) {
		const struct strmap_entry *e = &m->entries[(*it)++];
		if (e->key != nullptr) {
			if (key != nullptr) {
				*key = e->key;
			}
			if (value != nullptr) {
				*value = e->value;
			}
			return true;
		}
	}
	return false;
}

// The index slot pointing at entry ei
static size_t slot_of_entry(const struct strmap *m, size_t ei) {
	size_t mask = m->index_cap - 1;
	size_t i = (size_t)m->entries[ei].hash & mask;
	while (m->index[i] != ei + 1) {
		i = (i + 1) & mask;
	}
	return i;
}

// Drops removed entries at the end of the entry array
static void trim_removed(struct strmap *m) {
	while (m->n_entries > 0 && m->entries[m->n_entries - 1].key == nullptr) {
		m->n_entries--;
	}
}

void *strmap_swap_take(struct strmap *m, const char *key) {
	size_t slot = lookup(m, key);
	if (slot == SIZE_MAX) {
		return nullptr;
	}
	size_t ei = m->index[slot] - 1;
	struct strmap_entry *e = &m->entries[ei];
	void *value = e->value;
	xfree(e->key);
	*e = (struct strmap_entry){0};
	m->index[slot] = SLOT_REMOVED;
	m->live--;
	trim_removed(m);
	if (ei < m->n_entries) {
		// Move the last live entry into the hole
		size_t last = m->n_entries - 1;
		m->index[slot_of_entry(m, last)] = ei + 1;
		m->entries[ei] = m->entries[last];
		m->entries[last] = (struct strmap_entry){0};
		trim_removed(m);
	}
	return value;
}

bool strmap_swap_remove(struct strmap *m, const char *key) {
	if (!strmap_has(m, key)) {
		return false;
	}
	void *value = strmap_swap_take(m, key);
	if (m->free_value != nullptr) {
		m->free_value(value);
	}
	return true;
}

size_t strmap_capacity(const struct strmap *m) {
	return m->cap_entries;
}

void strmap_shrink_to_fit(struct strmap *m) {
	if (m->live == 0) {
		xfree(m->entries);
		xfree(m->index);
		*m = (struct strmap){.free_value = m->free_value};
		return;
	}
	rehash(m, m->live);
	m->cap_entries = m->n_entries;
	m->entries = xreallocarray(m->entries, m->cap_entries, sizeof(*m->entries));
}
