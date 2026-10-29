#include "service/service.h"
#include "base/alloc.h"
#include <math.h>
#include <stdlib.h>
#include <stdio.h>

bool service_parse_duration(const char *s, uint64_t *ns) {
	if (cstr_eq(s, "0")) {
		*ns = 0;
		return true;
	}
	if (*s == '+') {
		s++;
	}
	if (*s == '\0' || *s == '-') {
		return false;
	}
	long double total = 0;
	while (*s) {
		const char *start = s;
		while ((*s >= '0' && *s <= '9') || *s == '.') {
			s++;
		}
		if (s == start) {
			return false;
		}
		AUTOFREE char *number = str_dup(str_new(start, (size_t)(s - start)));
		char *end = nullptr;
		long double v = strtold(number, &end);
		if (*end || !isfinite(v)) {
			return false;
		}
		start = s;
		while (*s && !(*s >= '0' && *s <= '9') && *s != '.') {
			s++;
		}
		struct str unit = str_new(start, (size_t)(s - start));
		long double mult = 0;
		if (str_eq_cstr(unit, "ns")) {
			mult = 1;
		} else if (str_eq_cstr(unit, "us") || str_eq_cstr(unit, "µs") ||
			   str_eq_cstr(unit, "μs")) {
			mult = 1000;
		} else if (str_eq_cstr(unit, "ms")) {
			mult = 1000000;
		} else if (str_eq_cstr(unit, "s")) {
			mult = 1000000000;
		} else if (str_eq_cstr(unit, "m")) {
			mult = 60000000000.0L;
		} else if (str_eq_cstr(unit, "h")) {
			mult = 3600000000000.0L;
		} else {
			return false;
		}
		total += v * mult;
		if (total > (long double)INT64_MAX) {
			return false;
		}
	}
	*ns = (uint64_t)total;
	return true;
}
char *service_format_duration(uint64_t ns) {
	if (ns == 0) {
		return xstrdup("0s");
	}
	uint64_t secs = ns / 1000000000, h = secs / 3600, m = secs / 60 % 60, s = secs % 60;
	AUTO_STRBUF struct strbuf out = {0};
	if (h) {
		strbuf_printf(&out, "%lluh", (unsigned long long)h);
	}
	if (h || m) {
		strbuf_printf(&out, "%llum", (unsigned long long)m);
	}
	strbuf_printf(&out, "%llu", (unsigned long long)s);
	uint64_t frac = ns % 1000000000;
	if (frac) {
		char b[10];
		snprintf(b, sizeof(b), "%09llu", (unsigned long long)frac);
		size_t len = strlen(b);
		while (len && b[len - 1] == '0') {
			len--;
		}
		strbuf_append_char(&out, '.');
		strbuf_append(&out, b, len);
	}
	strbuf_append_char(&out, 's');
	return strbuf_take(&out);
}
char *service_redact_command(const char *raw) {
	struct irc_msg *m = irc_msg_parse(str_from(raw), nullptr);
	if (m == nullptr) {
		return xstrdup("(invalid)");
	}
	char *out = m->params.len > 1 ? xasprintf("%s %s (parameters hidden)", m->command,
						  irc_msg_param(m, 0))
		    : m->params.len   ? xasprintf("%s (parameters hidden)", m->command)
				      : xstrdup(m->command);
	irc_msg_free(m);
	return out;
}
