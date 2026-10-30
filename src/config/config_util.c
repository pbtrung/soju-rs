// Helpers of the directive handlers (config/config_directives.c)
#include "config/internal.h"

#include "base/alloc.h"
#include "base/utf8.h"
#include "config/flags.h"

// The loopback networks 127.0.0.0/8 and ::1/128
void cfg_push_loopback(struct ipset *s) {
	struct ipnet n;
	(void)ipnet_parse(STR_LIT("127.0.0.0/8"), &n);
	vec_push(&s->v, n);
	(void)ipnet_parse(STR_LIT("::1/128"), &n);
	vec_push(&s->v, n);
}

void cfg_get(const struct scfg_block *b, const char *name, dirvec *out) {
	vec_clear(out);
	for (size_t i = 0; i < b->v.len; i++) {
		if (cstr_eq(b->v.data[i].name, name)) {
			vec_push(out, &b->v.data[i]);
		}
	}
}

[[gnu::format(printf, 3, 4)]] bool cfg_dir_err(struct err **err, const struct scfg_directive *d,
					       const char *fmt, ...) {
	va_list ap;
	va_start(ap, fmt);
	AUTO_STRBUF struct strbuf msg = STRBUF_INIT;
	strbuf_vprintf(&msg, fmt, ap);
	va_end(ap);
	AUTOFREE char *q = utf8_debug_cstr(str_from(d->name));
	return err_set(err, 0, "line %zu, directive %s: %s", d->lineno, q, strbuf_cstr(&msg));
}

// The single directive with this name, if any. Mirrors go-scfg which
// rejects non-repeatable directives specified more than once.
bool cfg_single(const dirvec *dirs, const struct scfg_directive **out, struct err **err) {
	if (dirs->len > 1) {
		return cfg_dir_err(err, dirs->data[1],
				   "directive must not be specified more than once");
	}
	const struct scfg_directive *d = dirs->len > 0 ? dirs->data[0] : nullptr;
	if (d != nullptr && d->has_children) {
		return cfg_dir_err(err, d, "directive requires zero children");
	}
	*out = d;
	return true;
}

// *out is borrowed from the directive, nullptr if missing
bool cfg_single_string(const dirvec *dirs, const char **out, struct err **err) {
	const struct scfg_directive *d = nullptr;
	if (!cfg_single(dirs, &d, err)) {
		return false;
	}
	*out = nullptr;
	if (d != nullptr) {
		if (d->params.len != 1) {
			return cfg_dir_err(err, d, "directive requires exactly one parameter");
		}
		*out = d->params.data[0];
	}
	return true;
}

// *out: the parameters (borrowed), nullptr if missing
bool cfg_single_list(const dirvec *dirs, const strvec **out, struct err **err) {
	const struct scfg_directive *d = nullptr;
	if (!cfg_single(dirs, &d, err)) {
		return false;
	}
	*out = d != nullptr ? &d->params : nullptr;
	return true;
}

bool cfg_single_pair(const dirvec *dirs, const char **a, const char **b, struct err **err) {
	const struct scfg_directive *d = nullptr;
	if (!cfg_single(dirs, &d, err)) {
		return false;
	}
	*a = *b = nullptr;
	if (d != nullptr) {
		if (d->params.len != 2) {
			return cfg_dir_err(err, d, "directive requires exactly 2 parameters");
		}
		*a = d->params.data[0];
		*b = d->params.data[1];
	}
	return true;
}

bool cfg_parse_bool(const char *s, bool *out, struct err **err) {
	if (!parse_go_bool(str_from(s), out)) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(s));
		return err_set(err, 0, "strconv.ParseBool: parsing %s: invalid syntax", q);
	}
	return true;
}

// Borrowed driver and source ("" if missing)
bool cfg_parse_driver_source(const char *name, const strvec *params, const char **driver,
			     const char **source, struct err **err) {
	switch (params->len) {
	case 1:
		*driver = params->data[0];
		*source = "";
		return true;
	case 2:
		*driver = params->data[0];
		*source = params->data[1];
		return true;
	default:
		return err_set(err, 0, "directive %s requires exactly 1 or 2 parameters", name);
	}
}

bool parse_cidr(struct str s, struct ipnet *out, struct err **err) {
	if (!ipnet_parse(s, out)) {
		return err_set(err, 0, "invalid CIDR address: %.*s", STR_FMT(s));
	}
	*out = ipnet_trunc(*out);
	return true;
}

void cfg_replace(char **field, const char *value) {
	char *v = xstrdup(value);
	xfree(*field);
	*field = v;
}

// Wraps the error of a nested call: "<prefix>: <message>"
bool cfg_wrapped(struct err **err, struct err *e, const char *prefix) {
	err_set(err, 0, "%s: %s", prefix, err_msg(e));
	err_free(e);
	return false;
}
