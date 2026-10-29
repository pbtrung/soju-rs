#include "irc/xirc.h"

#include "base/alloc.h"
#include "base/utf8.h"

void cap_registry_init(struct cap_registry *r) {
	strmap_init(&r->available, xfree);
	strmap_init(&r->enabled, nullptr);
}

void cap_registry_free(struct cap_registry *r) {
	strmap_free(&r->available);
	strmap_free(&r->enabled);
}

bool cap_registry_is_available(const struct cap_registry *r, const char *name) {
	return strmap_has(&r->available, name);
}

bool cap_registry_is_enabled(const struct cap_registry *r, const char *name) {
	return strmap_has(&r->enabled, name);
}

const char *cap_registry_value(const struct cap_registry *r, const char *name) {
	return strmap_get(&r->available, name);
}

void cap_registry_set_available(struct cap_registry *r, const char *name, const char *value) {
	strmap_set(&r->available, name, xstrdup(value));
}

void cap_registry_del(struct cap_registry *r, const char *name) {
	strmap_remove(&r->available, name);
	strmap_remove(&r->enabled, name);
}

void cap_registry_set_enabled(struct cap_registry *r, const char *name, bool enabled) {
	if (enabled) {
		if (!strmap_has(&r->enabled, name)) {
			strmap_set(&r->enabled, name, nullptr);
		}
	} else {
		strmap_remove(&r->enabled, name);
	}
}

bool channel_status_parse(struct str s, enum channel_status *out, struct err **err) {
	if (s.len > 1) {
		AUTOFREE char *q = utf8_debug_cstr(s);
		return err_set(err, 0, "invalid channel status %s: more than one character", q);
	}
	if (str_eq_cstr(s, "=")) {
		*out = CHANNEL_PUBLIC;
	} else if (str_eq_cstr(s, "@")) {
		*out = CHANNEL_SECRET;
	} else if (str_eq_cstr(s, "*")) {
		*out = CHANNEL_PRIVATE;
	} else {
		AUTOFREE char *q = utf8_debug_cstr(s);
		return err_set(err, 0, "invalid channel status %s: unknown status", q);
	}
	return true;
}

const char *channel_status_str(enum channel_status s) {
	switch (s) {
	case CHANNEL_PUBLIC:
		return "=";
	case CHANNEL_SECRET:
		return "@";
	case CHANNEL_PRIVATE:
		return "*";
	}
	return "=";
}

void membership_set_add(struct membership_set *ms, const struct membership *available,
			size_t n_available, struct membership m) {
	size_t i = 0;
	for (size_t k = 0; k < n_available; k++) {
		struct membership avail = available[k];
		if (i >= ms->v.len) {
			break;
		}
		if (membership_eq(ms->v.data[i], avail)) {
			if (membership_eq(avail, m)) {
				// we already have this membership
				return;
			}
			i++;
			continue;
		}
		if (membership_eq(avail, m)) {
			break;
		}
	}
	vec_insert(&ms->v, i, m);
}

void membership_set_remove(struct membership_set *ms, struct membership m) {
	for (size_t i = 0; i < ms->v.len; i++) {
		if (membership_eq(ms->v.data[i], m)) {
			vec_remove(&ms->v, i);
			return;
		}
	}
}

void membership_set_free(struct membership_set *ms) {
	vec_free(&ms->v);
}

void replace_all(struct strbuf *out, struct str s, const struct replace_pair *pairs, size_t n) {
	strbuf_reserve(out, s.len);
	size_t i = 0;
	while (i < s.len) {
		struct str rest = str_slice(s, i, s.len);
		bool replaced = false;
		for (size_t k = 0; k < n; k++) {
			struct str old = str_from(pairs[k].old);
			if (old.len > 0 && str_starts_with(rest, old)) {
				strbuf_append_cstr(out, pairs[k].new);
				i += old.len;
				replaced = true;
				break;
			}
		}
		if (!replaced) {
			// One character at a time, like the Rust code
			size_t j = i;
			uint32_t cp;
			(void)utf8_next(s, &j, &cp);
			strbuf_append(out, s.p + i, j - i);
			i = j;
		}
	}
}

void decode_isupport_value(struct strbuf *out, struct str v) {
	static const struct replace_pair pairs[] = {{"\\x20", " "}, {"\\x5C", "\\"}};
	replace_all(out, v, pairs, 2);
}

void encode_isupport_value(struct strbuf *out, struct str v) {
	static const struct replace_pair pairs[] = {{" ", "\\x20"}, {"\\", "\\x5C"}};
	replace_all(out, v, pairs, 2);
}

void isupport_init(struct isupport *is) {
	strmap_init(&is->m, xfree);
}

void isupport_free(struct isupport *is) {
	strmap_free(&is->m);
}

void isupport_set(struct isupport *is, const char *name, const char *value) {
	strmap_set(&is->m, name, value != nullptr ? xstrdup(value) : nullptr);
}
