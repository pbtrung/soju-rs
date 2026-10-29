#include "irc/casemapping.h"

#include "base/alloc.h"

char *casemap(enum casemapping cm, struct str name) {
	char *out = xmalloc(size_add(name.len, 1));
	for (size_t i = 0; i < name.len; i++) {
		char c = name.p[i];
		if (c >= 'A' && c <= 'Z') {
			c = (char)(c - 'A' + 'a');
		} else if (cm != CASEMAPPING_ASCII) {
			switch (c) {
			case '{':
				c = '[';
				break;
			case '}':
				c = ']';
				break;
			case '\\':
				c = '|';
				break;
			case '~':
				if (cm == CASEMAPPING_RFC1459) {
					c = '^';
				}
				break;
			default:
				break;
			}
		}
		out[i] = c;
	}
	out[name.len] = '\0';
	return out;
}

bool parse_casemapping(struct str s, enum casemapping *out) {
	if (str_eq_cstr(s, "ascii")) {
		*out = CASEMAPPING_ASCII;
	} else if (str_eq_cstr(s, "rfc1459")) {
		*out = CASEMAPPING_RFC1459;
	} else if (str_eq_cstr(s, "rfc1459-strict")) {
		*out = CASEMAPPING_RFC1459_STRICT;
	} else {
		return false;
	}
	return true;
}

struct cmmap_entry {
	char *name; // the original spelling, if different from the key
	void *value;
	void (*free_value)(void *value);
};

static void entry_free(void *p) {
	struct cmmap_entry *e = p;
	if (e == nullptr) {
		return;
	}
	if (e->free_value != nullptr) {
		e->free_value(e->value);
	}
	xfree(e->name);
	xfree(e);
}

static struct cmmap_entry *entry_new(const char *cm, const char *name, void *value,
				     void (*free_value)(void *)) {
	struct cmmap_entry *e = xmalloc(sizeof(*e));
	*e = (struct cmmap_entry){
		.name = cstr_eq(cm, name) ? nullptr : xstrdup(name),
		.value = value,
		.free_value = free_value,
	};
	return e;
}

void cmmap_init(struct cmmap *m, enum casemapping cm, void (*free_value)(void *value)) {
	strmap_init(&m->m, entry_free);
	m->cm = cm;
	m->free_value = free_value;
}

void cmmap_free(struct cmmap *m) {
	strmap_free(&m->m);
}

void cmmap_clear(struct cmmap *m) {
	strmap_clear(&m->m);
}

size_t cmmap_len(const struct cmmap *m) {
	return strmap_len(&m->m);
}

static struct cmmap_entry *get_entry(const struct cmmap *m, const char *name) {
	AUTOFREE char *cm = casemap_cstr(m->cm, name);
	return strmap_get(&m->m, cm);
}

bool cmmap_has(const struct cmmap *m, const char *name) {
	return get_entry(m, name) != nullptr;
}

void *cmmap_get(const struct cmmap *m, const char *name) {
	struct cmmap_entry *e = get_entry(m, name);
	return e != nullptr ? e->value : nullptr;
}

void cmmap_set(struct cmmap *m, const char *name, void *value) {
	AUTOFREE char *cm = casemap_cstr(m->cm, name);
	struct cmmap_entry *e = strmap_get(&m->m, cm);
	if (e != nullptr) {
		if (m->free_value != nullptr && e->value != value) {
			m->free_value(e->value);
		}
		e->value = value;
		return;
	}
	strmap_set(&m->m, cm, entry_new(cm, name, value, m->free_value));
}

void *cmmap_take(struct cmmap *m, const char *name) {
	AUTOFREE char *cm = casemap_cstr(m->cm, name);
	struct cmmap_entry *e = strmap_swap_take(&m->m, cm);
	if (strmap_capacity(&m->m) > 64 && strmap_len(&m->m) * 4 < strmap_capacity(&m->m)) {
		strmap_shrink_to_fit(&m->m);
	}
	if (e == nullptr) {
		return nullptr;
	}
	void *value = e->value;
	e->free_value = nullptr;
	entry_free(e);
	return value;
}

bool cmmap_del(struct cmmap *m, const char *name) {
	if (!cmmap_has(m, name)) {
		return false;
	}
	void *value = cmmap_take(m, name);
	if (m->free_value != nullptr) {
		m->free_value(value);
	}
	return true;
}

bool cmmap_next(const struct cmmap *m, size_t *it, const char **name, void **value) {
	const char *key;
	void *v;
	if (!strmap_next(&m->m, it, &key, &v)) {
		return false;
	}
	const struct cmmap_entry *e = v;
	if (name != nullptr) {
		*name = e->name != nullptr ? e->name : key;
	}
	if (value != nullptr) {
		*value = e->value;
	}
	return true;
}

void **cmmap_value_slot(const struct cmmap *m, size_t it) {
	struct cmmap_entry *e = m->m.entries[it - 1].value;
	return &e->value;
}

void cmmap_set_casemapping(struct cmmap *m, enum casemapping cm) {
	struct strmap old = m->m;
	m->cm = cm;
	strmap_init(&m->m, entry_free);
	const char *key;
	void *v;
	for (size_t it = 0; strmap_next(&old, &it, &key, &v);) {
		struct cmmap_entry *e = v;
		const char *name = e->name != nullptr ? e->name : key;
		AUTOFREE char *new_cm = casemap_cstr(cm, name);
		struct cmmap_entry *ne = entry_new(new_cm, name, e->value, m->free_value);
		// Like IndexMap::insert: a colliding key keeps its position, and
		// takes the new entry (spelling and value)
		struct cmmap_entry *prev = strmap_get(&m->m, new_cm);
		if (prev != nullptr) {
			if (m->free_value != nullptr) {
				m->free_value(prev->value);
			}
			prev->value = e->value;
			xfree(prev->name);
			prev->name = ne->name;
			ne->name = nullptr;
			ne->free_value = nullptr;
			entry_free(ne);
		} else {
			strmap_set(&m->m, new_cm, ne);
		}
		e->free_value = nullptr;
	}
	strmap_free(&old);
}

bool cmmap_peek_spelling(const struct cmmap *m, const char *cm_key, const char **spelling) {
	const struct cmmap_entry *e = strmap_get(&m->m, cm_key);
	if (e == nullptr) {
		return false;
	}
	*spelling = e->name;
	return true;
}

size_t cmmap_capacity(const struct cmmap *m) {
	return strmap_capacity(&m->m);
}
