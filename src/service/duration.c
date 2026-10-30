#include "service/service.h"
#include "base/alloc.h"
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
	uint64_t total = 0;
	while (*s) {
		uint64_t whole = 0;
		bool digits = false;
		while (*s >= '0' && *s <= '9') {
			unsigned d = (unsigned)(*s++ - '0');
			if (whole > ((uint64_t)INT64_MAX - d) / 10) {
				return false;
			}
			whole = whole * 10 + d;
			digits = true;
		}
		const char *fraction = s, *end = s;
		if (*s == '.') {
			fraction = ++s;
			while (*s >= '0' && *s <= '9') {
				s++;
				digits = true;
			}
			end = s;
		}
		if (!digits) {
			return false;
		}
		const char *start = s;
		while (*s && !(*s >= '0' && *s <= '9') && *s != '.') {
			s++;
		}
		struct str unit = str_new(start, (size_t)(s - start));
		uint64_t mult;
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
			mult = 60000000000ULL;
		} else if (str_eq_cstr(unit, "h")) {
			mult = 3600000000000ULL;
		} else {
			return false;
		}
		// Decimal multiplication from the least significant digit avoids
		// floating point rounding and cannot overflow (carry < mult).
		uint64_t carry = 0;
		while (end > fraction) {
			carry = ((unsigned)(*--end - '0') * mult + carry) / 10;
		}
		if (carry > (uint64_t)INT64_MAX - total ||
		    whole > ((uint64_t)INT64_MAX - total - carry) / mult) {
			return false;
		}
		total += whole * mult + carry;
	}
	*ns = total;
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
