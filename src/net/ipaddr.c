#include "net/ipaddr.h"

#include <arpa/inet.h>
#include <string.h>

struct parser {
	struct str s;
	size_t pos;
};

static bool eof(const struct parser *p) {
	return p->pos == p->s.len;
}

static bool read_char(struct parser *p, char c) {
	if (!eof(p) && p->s.p[p->pos] == c) {
		p->pos++;
		return true;
	}
	return false;
}

static int digit(char c, unsigned radix) {
	if (c >= '0' && c <= '9') {
		return c - '0';
	}
	if (radix == 16 && c >= 'a' && c <= 'f') {
		return c - 'a' + 10;
	}
	if (radix == 16 && c >= 'A' && c <= 'F') {
		return c - 'A' + 10;
	}
	return -1;
}

// Reads a number below upto with at most max_digits digits (0: no limit).
// Without allow_zero_prefix, "0" followed by digits is rejected (std's
// IPv4 octets). On failure, the position is left unchanged.
static bool read_number(struct parser *p, unsigned radix, unsigned max_digits, uint64_t upto,
			bool allow_zero_prefix, uint32_t *out) {
	size_t start = p->pos;
	uint64_t r = 0;
	unsigned n = 0;
	bool leading_zero = !eof(p) && p->s.p[p->pos] == '0';
	while (!eof(p)) {
		int d = digit(p->s.p[p->pos], radix);
		if (d < 0) {
			break;
		}
		p->pos++;
		r = r * radix + (uint64_t)d;
		n++;
		if ((max_digits != 0 && n > max_digits) || r >= upto ||
		    (!allow_zero_prefix && leading_zero && n > 1)) {
			p->pos = start;
			return false;
		}
	}
	if (n == 0) {
		p->pos = start;
		return false;
	}
	*out = (uint32_t)r;
	return true;
}

static bool read_ipv4(struct parser *p, bool std_rules, uint8_t b[4]) {
	size_t start = p->pos;
	for (int i = 0; i < 4; i++) {
		uint32_t v;
		if ((i != 0 && !read_char(p, '.')) ||
		    !read_number(p, 10, 3, 0x100, !std_rules, &v)) {
			p->pos = start;
			return false;
		}
		b[i] = (uint8_t)v;
	}
	return true;
}

// Reads up to limit groups (with a trailing embedded IPv4 address when two
// groups are left); returns how many, and whether IPv4 ended them
static size_t read_groups(struct parser *p, bool std_rules, uint16_t *groups, size_t limit,
			  bool *ipv4) {
	*ipv4 = false;
	for (size_t i = 0; i < limit; i++) {
		if (i + 1 < limit) {
			size_t start = p->pos;
			uint8_t b[4];
			if ((i == 0 || read_char(p, ':')) && read_ipv4(p, std_rules, b)) {
				groups[i] = (uint16_t)(((unsigned)b[0] << 8U) | b[1]);
				groups[i + 1] = (uint16_t)(((unsigned)b[2] << 8U) | b[3]);
				*ipv4 = true;
				return i + 2;
			}
			p->pos = start;
		}
		size_t start = p->pos;
		uint32_t g;
		if (!((i == 0 || read_char(p, ':')) && read_number(p, 16, 4, 0x10000, true, &g))) {
			p->pos = start;
			return i;
		}
		groups[i] = (uint16_t)g;
	}
	return limit;
}

static bool read_ipv6(struct parser *p, bool std_rules, uint8_t out[16]) {
	size_t start = p->pos;
	uint16_t head[8] = {0}, tail[8] = {0}, gs[8] = {0};
	bool ipv4;
	size_t head_n = read_groups(p, std_rules, head, 8, &ipv4);
	if (head_n == 8) {
		memcpy(gs, head, sizeof(gs));
	} else {
		// IPv4 part is not allowed before "::"
		if (ipv4 || !read_char(p, ':') || !read_char(p, ':')) {
			p->pos = start;
			return false;
		}
		// std: "::" stands for at least one group; the old parser ipnet
		// copied allows a full tail
		size_t limit = std_rules ? 8 - (head_n + 1) : 8 - head_n;
		size_t tail_n = read_groups(p, std_rules, tail, limit, &ipv4);
		memcpy(gs, head, head_n * sizeof(uint16_t));
		memcpy(gs + (8 - tail_n), tail, tail_n * sizeof(uint16_t));
	}
	for (size_t i = 0; i < 8; i++) {
		out[2 * i] = (uint8_t)(gs[i] >> 8U);
		out[2 * i + 1] = (uint8_t)gs[i];
	}
	return true;
}

static bool read_ip(struct parser *p, bool std_rules, struct ipaddr *a) {
	*a = (struct ipaddr){0};
	size_t start = p->pos;
	if (read_ipv4(p, std_rules, a->b) && eof(p)) {
		return true;
	}
	p->pos = start;
	*a = (struct ipaddr){.v6 = true};
	if (read_ipv6(p, std_rules, a->b) && eof(p)) {
		return true;
	}
	p->pos = start;
	return false;
}

bool ipaddr_parse(struct str s, struct ipaddr *out) {
	struct parser p = {.s = s};
	return read_ip(&p, true, out);
}

bool socketaddr_parse(struct str s, struct ipaddr *addr, uint16_t *port) {
	struct parser p = {.s = s};
	uint32_t v;
	// SocketAddrV4: ip ':' port
	*addr = (struct ipaddr){0};
	if (read_ipv4(&p, true, addr->b) && read_char(&p, ':') &&
	    read_number(&p, 10, 0, 0x10000, true, &v) && eof(&p)) {
		*port = (uint16_t)v;
		return true;
	}
	// SocketAddrV6: '[' ip ('%' scope)? ']' ':' port
	p.pos = 0;
	*addr = (struct ipaddr){.v6 = true};
	uint32_t scope;
	if (read_char(&p, '[') && read_ipv6(&p, true, addr->b) &&
	    (!read_char(&p, '%') || read_number(&p, 10, 0, 0x100000000ULL, true, &scope)) &&
	    read_char(&p, ']') && read_char(&p, ':') && read_number(&p, 10, 0, 0x10000, true, &v) &&
	    eof(&p)) {
		*port = (uint16_t)v;
		return true;
	}
	return false;
}

struct ipaddr ipaddr_canonical(struct ipaddr a) {
	static const uint8_t mapped[12] = {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF};
	if (a.v6 && memcmp(a.b, mapped, 12) == 0) {
		struct ipaddr v4 = {0};
		memcpy(v4.b, a.b + 12, 4);
		return v4;
	}
	return a;
}

void ipaddr_format(struct strbuf *out, struct ipaddr a) {
	char buf[INET6_ADDRSTRLEN];
	// inet_ntop follows RFC 5952 like Rust, but writes IPv4-compatible
	// addresses differently: Rust only uses the dotted form for
	// IPv4-mapped ones
	if (!a.v6) {
		inet_ntop(AF_INET, a.b, buf, sizeof(buf));
		strbuf_append_cstr(out, buf);
		return;
	}
	static const uint8_t mapped[12] = {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF};
	if (memcmp(a.b, mapped, 12) == 0) {
		strbuf_printf(out, "::ffff:%u.%u.%u.%u", a.b[12], a.b[13], a.b[14], a.b[15]);
		return;
	}
	uint16_t g[8];
	for (size_t i = 0; i < 8; i++) {
		g[i] = (uint16_t)(((unsigned)a.b[2 * i] << 8U) | a.b[2 * i + 1]);
	}
	// The longest run of two or more zero groups, the first one on ties
	int best = -1, best_len = 0;
	for (int i = 0; i < 8;) {
		if (g[i] != 0) {
			i++;
			continue;
		}
		int j = i;
		while (j < 8 && g[j] == 0) {
			j++;
		}
		if (j - i > best_len && j - i >= 2) {
			best = i;
			best_len = j - i;
		}
		i = j;
	}
	if (best < 0) {
		for (int i = 0; i < 8; i++) {
			strbuf_printf(out, i > 0 ? ":%x" : "%x", g[i]);
		}
		return;
	}
	for (int i = 0; i < best; i++) {
		strbuf_printf(out, i > 0 ? ":%x" : "%x", g[i]);
	}
	strbuf_append_cstr(out, "::");
	for (int i = best + best_len; i < 8; i++) {
		strbuf_printf(out, i > best + best_len ? ":%x" : "%x", g[i]);
	}
}

bool ipnet_parse(struct str s, struct ipnet *out) {
	struct parser p = {.s = s};
	uint32_t prefix;
	*out = (struct ipnet){0};
	if (read_ipv4(&p, false, out->addr.b) && read_char(&p, '/') &&
	    read_number(&p, 10, 2, 33, true, &prefix) && eof(&p)) {
		out->prefix = (uint8_t)prefix;
		return true;
	}
	p.pos = 0;
	*out = (struct ipnet){.addr.v6 = true};
	if (read_ipv6(&p, false, out->addr.b) && read_char(&p, '/') &&
	    read_number(&p, 10, 3, 129, true, &prefix) && eof(&p)) {
		out->prefix = (uint8_t)prefix;
		return true;
	}
	return false;
}

struct ipnet ipnet_trunc(struct ipnet n) {
	size_t nbytes = n.addr.v6 ? 16 : 4;
	for (size_t i = 0; i < nbytes; i++) {
		size_t bit = i * 8;
		if (bit >= n.prefix) {
			n.addr.b[i] = 0;
		} else if (bit + 8 > n.prefix) {
			n.addr.b[i] &= (uint8_t)(0xFFU << (8 - (n.prefix - bit)));
		}
	}
	return n;
}

bool ipnet_contains(struct ipnet n, struct ipaddr a) {
	if (n.addr.v6 != a.v6) {
		return false;
	}
	struct ipnet t = ipnet_trunc(n);
	struct ipnet b = {.addr = a, .prefix = n.prefix};
	b = ipnet_trunc(b);
	return memcmp(t.addr.b, b.addr.b, 16) == 0;
}
