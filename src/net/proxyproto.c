#include "net/proxyproto.h"

#include <string.h>

#include "base/utf8.h"

static const char V1_SIGNATURE[] = "PROXY ";
static const char V2_SIGNATURE[] = "\r\n\r\n\0\r\nQUIT\n";
static constexpr size_t V1_SIG_LEN = sizeof(V1_SIGNATURE) - 1;
static constexpr size_t V2_SIG_LEN = sizeof(V2_SIGNATURE) - 1;
static constexpr size_t V1_MAX_LEN = 107;
static constexpr size_t V2_MAX_LEN = 4096;
static constexpr uint8_t PP2_TYPE_SSL = 0x20;
static constexpr uint8_t PP2_SUBTYPE_SSL_CLIENT_CERT = 0x28;

static_assert(V2_SIG_LEN == 12);

void proxy_header_free(struct proxy_header *h) {
	strbuf_free(&h->client_cert);
	*h = (struct proxy_header){0};
}

static bool prefix_of(struct str buf, const char *sig, size_t sig_len) {
	size_t n = buf.len < sig_len ? buf.len : sig_len;
	return memcmp(buf.p, sig, n) == 0;
}

static bool parse_port(struct str s, uint16_t *out, struct err **err) {
	unsigned long long v;
	if (s.len == 0) {
		return err_set(err, 0, "cannot parse integer from empty string");
	}
	if (!str_parse_u64(s, &v)) {
		return err_set(err, 0, "invalid digit found in string");
	}
	if (v > 65535) {
		return err_set(err, 0, "number too large to fit in target type");
	}
	*out = (uint16_t)v;
	return true;
}

static enum proxy_result parse_v1(struct str buf, bool eof, struct proxy_header *h,
				  size_t *consumed, struct err **err) {
	// Bytes after the signature, one at a time: stop at "\r\n", fail once
	// too long
	size_t end = 0;
	for (size_t i = V1_SIG_LEN; i < buf.len; i++) {
		if (i >= 1 && buf.p[i - 1] == '\r' && buf.p[i] == '\n') {
			end = i + 1;
			break;
		}
		if (i + 1 > V1_MAX_LEN) {
			err_set(err, 0, "proxyproto: v1 header too long");
			return PROXY_ERROR;
		}
	}
	if (end == 0) {
		if (eof) {
			err_set(err, 0, "proxyproto: unexpected EOF in v1 header");
			return PROXY_ERROR;
		}
		return PROXY_NEED_MORE;
	}
	struct str line = str_slice(buf, 0, end - 2);
	if (!utf8_valid(line)) {
		err_set(err, 0, "invalid utf-8 sequence");
		return PROXY_ERROR;
	}
	struct str parts[7];
	size_t n = 0;
	struct str rest = line, part;
	while (str_next_field(&rest, ' ', &part)) {
		if (n < 7) {
			parts[n] = part;
		}
		n++;
	}
	*h = (struct proxy_header){0};
	h->present = true;
	if (n >= 2 && str_eq_cstr(parts[1], "UNKNOWN")) {
		*consumed = end;
		return PROXY_HEADER;
	}
	if (n != 6 || (!str_eq_cstr(parts[1], "TCP4") && !str_eq_cstr(parts[1], "TCP6"))) {
		err_set(err, 0, "proxyproto: invalid v1 header");
		return PROXY_ERROR;
	}
	if (!ipaddr_parse(parts[2], &h->source_ip) || !ipaddr_parse(parts[3], &h->destination_ip)) {
		err_set(err, 0, "invalid IP address syntax");
		return PROXY_ERROR;
	}
	if (!parse_port(parts[4], &h->source_port, err) ||
	    !parse_port(parts[5], &h->destination_port, err)) {
		return PROXY_ERROR;
	}
	h->has_source = true;
	*consumed = end;
	return PROXY_HEADER;
}

static uint16_t be16(const char *p) {
	return (uint16_t)(((unsigned)(unsigned char)p[0] << 8U) | (unsigned char)p[1]);
}

// Calls back for each TLV; false on a malformed one
static bool tlvs(struct str b, bool (*f)(uint8_t type, struct str value, void *data), void *data,
		 struct err **err) {
	while (b.len > 0) {
		if (b.len < 3) {
			return err_set(err, 0, "proxyproto: malformed TLV");
		}
		uint8_t type = (uint8_t)b.p[0];
		size_t len = be16(b.p + 1);
		if (b.len < 3 + len) {
			return err_set(err, 0, "proxyproto: malformed TLV");
		}
		if (!f(type, str_slice(b, 3, 3 + len), data)) {
			return false;
		}
		b = str_slice(b, 3 + len, b.len);
	}
	return true;
}

struct ssl_ctx {
	struct proxy_header *h;
	struct err **err;
};

static bool on_ssl_sub(uint8_t type, struct str v, void *data) {
	struct proxy_header *h = data;
	if (type == PP2_SUBTYPE_SSL_CLIENT_CERT) {
		strbuf_reset(&h->client_cert);
		strbuf_append_str(&h->client_cert, v);
		strbuf_append(&h->client_cert, "", 0);
		h->has_client_cert = true;
	}
	return true;
}

static bool on_tlv(uint8_t type, struct str v, void *data) {
	struct ssl_ctx *c = data;
	if (type == PP2_TYPE_SSL && v.len >= 5) {
		return tlvs(str_slice(v, 5, v.len), on_ssl_sub, c->h, c->err);
	}
	return true;
}

static enum proxy_result parse_v2(struct str buf, bool eof, struct proxy_header *h,
				  size_t *consumed, struct err **err) {
	struct str rest = str_slice(buf, V2_SIG_LEN, buf.len);
	if (rest.len < 4) {
		goto more;
	}
	uint8_t ver_cmd = (uint8_t)rest.p[0], fam = (uint8_t)rest.p[1];
	size_t len = be16(rest.p + 2);
	if (ver_cmd >> 4U != 2) {
		err_set(err, 0, "proxyproto: unsupported version");
		return PROXY_ERROR;
	}
	if (len > V2_MAX_LEN) {
		err_set(err, 0, "proxyproto: v2 header too long");
		return PROXY_ERROR;
	}
	if (rest.len < 4 + len) {
		goto more;
	}
	struct str d = str_slice(rest, 4, 4 + len);
	*h = (struct proxy_header){.present = true};
	bool local = (ver_cmd & 0x0FU) == 0;
	size_t addr_len;
	switch (fam >> 4U) {
	case 1:
		if (d.len < 12) {
			goto invalid;
		}
		addr_len = 12;
		memcpy(h->source_ip.b, d.p, 4);
		memcpy(h->destination_ip.b, d.p + 4, 4);
		h->source_port = be16(d.p + 8);
		h->destination_port = be16(d.p + 10);
		h->has_source = !local;
		break;
	case 2:
		if (d.len < 36) {
			goto invalid;
		}
		addr_len = 36;
		h->source_ip.v6 = h->destination_ip.v6 = true;
		memcpy(h->source_ip.b, d.p, 16);
		memcpy(h->destination_ip.b, d.p + 16, 16);
		h->source_port = be16(d.p + 32);
		h->destination_port = be16(d.p + 34);
		h->has_source = !local;
		break;
	case 3:
		if (d.len < 216) {
			goto invalid;
		}
		addr_len = 216;
		break;
	case 0:
		addr_len = 0;
		break;
	default:
		goto invalid;
	}
	if (!h->has_source) {
		h->source_ip = h->destination_ip = (struct ipaddr){0};
		h->source_port = h->destination_port = 0;
	}
	struct ssl_ctx c = {h, err};
	if (!tlvs(str_slice(d, addr_len, d.len), on_tlv, &c, err)) {
		proxy_header_free(h);
		return PROXY_ERROR;
	}
	*consumed = V2_SIG_LEN + 4 + len;
	return PROXY_HEADER;
invalid:
	err_set(err, 0, "proxyproto: invalid v2 address block");
	return PROXY_ERROR;
more:
	if (eof) {
		err_set(err, 0, "early eof");
		return PROXY_ERROR;
	}
	return PROXY_NEED_MORE;
}

enum proxy_result proxy_parse(struct str buf, bool eof, struct proxy_header *h, size_t *consumed,
			      struct err **err) {
	bool v1 = prefix_of(buf, V1_SIGNATURE, V1_SIG_LEN);
	bool v2 = prefix_of(buf, V2_SIGNATURE, V2_SIG_LEN);
	if (v1 && buf.len >= V1_SIG_LEN) {
		return parse_v1(buf, eof, h, consumed, err);
	}
	if (v2 && buf.len >= V2_SIG_LEN) {
		return parse_v2(buf, eof, h, consumed, err);
	}
	if (v1 || v2) {
		// EOF: let the caller deal with it
		return eof ? PROXY_NONE : PROXY_NEED_MORE;
	}
	return PROXY_NONE;
}
