#include "db/utils.h"

static bool is_hex(char c) {
	return (c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F');
}

static bool is_digit(char c) {
	return c >= '0' && c <= '9';
}

static bool starts_hex_color(struct str s, size_t i) {
	if (s.len < i || s.len - i < 6) {
		return false;
	}
	for (size_t k = 0; k < 6; k++) {
		if (!is_hex(s.p[i + k])) {
			return false;
		}
	}
	return true;
}

void strip_ansi(struct strbuf *out, struct str s) {
	strbuf_reserve(out, s.len);
	const char *b = s.p;
	size_t len = s.len;
	for (size_t i = 0; i < len; i++) {
		switch (b[i]) {
		case 0x02:
		case 0x1D:
		case 0x1F:
		case 0x1E:
		case 0x11:
		case 0x16:
		case 0x0F:
			break;
		case 0x03:
			if (len > i + 1 && is_digit(b[i + 1])) {
				i++;
				if (len > i + 1 && is_digit(b[i + 1])) {
					i++;
				}
				if (len > i + 2 && b[i + 1] == ',' && is_digit(b[i + 2])) {
					i += 2;
					if (len > i + 1 && is_digit(b[i + 1])) {
						i++;
					}
				}
			}
			break;
		case 0x04:
			if (starts_hex_color(s, i + 1)) {
				i += 6;
				if (len > i + 1 && b[i + 1] == ',' && starts_hex_color(s, i + 2)) {
					i += 7;
				}
			}
			break;
		default:
			strbuf_append_char(out, b[i]);
		}
	}
}
