#include "config/flags.h"

#include <stdint.h>

#include "base/alloc.h"
#include "base/utf8.h"

bool parse_go_bool(struct str s, bool *out) {
	static const char *const t[] = {"1", "t", "T", "TRUE", "true", "True"};
	static const char *const f[] = {"0", "f", "F", "FALSE", "false", "False"};
	for (size_t i = 0; i < 6; i++) {
		if (str_eq_cstr(s, t[i])) {
			*out = true;
			return true;
		}
		if (str_eq_cstr(s, f[i])) {
			*out = false;
			return true;
		}
	}
	return false;
}

void flag_set_init(struct flag_set *fs) {
	strmap_init(&fs->flags, nullptr);
}

void flag_set_free(struct flag_set *fs) {
	strmap_free(&fs->flags);
}

void flag_set_add(struct flag_set *fs, const char *name, enum flag_kind kind) {
	strmap_set(&fs->flags, name, (void *)(uintptr_t)(kind + 1));
}

static void free_strvec(void *p) {
	strvec *v = p;
	if (v != nullptr) {
		strvec_free(v);
		xfree(v);
	}
}

void flags_parsed_free(struct flags_parsed *p) {
	strmap_free(&p->values);
	strvec_free(&p->args);
}

const strvec *flags_get_all(const struct flags_parsed *p, const char *name) {
	return strmap_get(&p->values, name);
}

const char *flags_get(const struct flags_parsed *p, const char *name) {
	const strvec *v = flags_get_all(p, name);
	return v != nullptr && v->len > 0 ? v->data[v->len - 1] : nullptr;
}

bool flags_get_bool(const struct flags_parsed *p, const char *name, bool *out) {
	const char *v = flags_get(p, name);
	return v != nullptr && parse_go_bool(str_from(v), out);
}

bool flags_get_int(const struct flags_parsed *p, const char *name, long long *out) {
	const char *v = flags_get(p, name);
	return v != nullptr && str_parse_i64(str_from(v), out);
}

#define FAIL(code, ...)                                   \
	do {                                              \
		flags_parsed_free(&p);                    \
		return err_set(err, (code), __VA_ARGS__); \
	} while (0)

bool flag_set_parse(const struct flag_set *fs, const char *const *args, size_t n,
		    struct flags_parsed *out, struct err **err) {
	struct flags_parsed p = {0};
	strmap_init(&p.values, free_strvec);
	size_t i = 0;
	while (i < n) {
		const char *s = args[i];
		size_t sl = strlen(s);
		if (sl < 2 || s[0] != '-') {
			break;
		}
		size_t num_minuses = 1;
		if (s[1] == '-') {
			num_minuses++;
			if (sl == 2) {
				// "--" terminates the flags
				i++;
				break;
			}
		}
		const char *name = s + num_minuses;
		if (name[0] == '\0' || name[0] == '-' || name[0] == '=') {
			FAIL(0, "bad flag syntax: %s", s);
		}
		i++;

		struct str nm = str_from(name), value = STR_EMPTY;
		bool has_value = str_split_once(str_from(name), '=', &nm, &value);
		AUTOFREE char *name_s = str_dup(nm);

		uintptr_t k = (uintptr_t)strmap_get(&fs->flags, name_s);
		if (k == 0) {
			if (cstr_eq(name_s, "help") || cstr_eq(name_s, "h")) {
				FAIL(FLAG_ERR_HELP, "flag: help requested");
			}
			FAIL(0, "flag provided but not defined: -%s", name_s);
		}
		enum flag_kind kind = (enum flag_kind)(k - 1);

		AUTOFREE char *v = nullptr;
		if (kind == FLAG_BOOL) {
			if (has_value) {
				bool b;
				if (!parse_go_bool(value, &b)) {
					AUTOFREE char *q = utf8_debug_cstr(value);
					FAIL(0, "invalid boolean value %s for -%s: parse error", q,
					     name_s);
				}
				v = str_dup(value);
			} else {
				v = xstrdup("true");
			}
		} else if (has_value) {
			v = str_dup(value);
		} else if (i < n) {
			v = xstrdup(args[i++]);
		} else {
			FAIL(0, "flag needs an argument: -%s", name_s);
		}

		bool b;
		long long ll;
		AUTOFREE char *q = utf8_debug_cstr(str_from(v));
		switch (kind) {
		case FLAG_BOOL_VALUE:
			if (!parse_go_bool(str_from(v), &b)) {
				FAIL(0,
				     "invalid value %s for flag -%s: strconv.ParseBool: parsing "
				     "%s: "
				     "invalid syntax",
				     q, name_s, q);
			}
			break;
		case FLAG_INT:
			if (!str_parse_i64(str_from(v), &ll)) {
				FAIL(0, "invalid value %s for flag -%s: parse error", q, name_s);
			}
			break;
		case FLAG_ATOI:
			if (!str_parse_i64(str_from(v), &ll)) {
				FAIL(0,
				     "invalid value %s for flag -%s: strconv.Atoi: parsing %s: "
				     "invalid syntax",
				     q, name_s, q);
			}
			break;
		default:
			break;
		}

		strvec *vals = strmap_get(&p.values, name_s);
		if (vals == nullptr) {
			vals = xcalloc(1, sizeof(*vals));
			strmap_set(&p.values, name_s, vals);
		}
		strvec_push_take(vals, v);
		v = nullptr;
	}
	for (; i < n; i++) {
		strvec_push(&p.args, args[i]);
	}
	*out = p;
	return true;
}
