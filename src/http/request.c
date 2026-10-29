#include "http/request.h"

#include <stdint.h>
#include <strings.h>

#include "base/alloc.h"

void http_request_free(struct http_request *req) {
	xfree(req->method);
	xfree(req->target);
	for (size_t i = 0; i < req->headers.len; i++) {
		xfree(req->headers.data[i].name);
		xfree(req->headers.data[i].value);
	}
	vec_free(&req->headers);
	*req = (struct http_request){0};
}

// RFC 9110 tchar
static bool is_tchar(char c) {
	return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
	       (c != '\0' && strchr("!#$%&'*+-.^_`|~", c) != nullptr);
}

static bool is_token(struct str s) {
	if (s.len == 0) {
		return false;
	}
	for (size_t i = 0; i < s.len; i++) {
		if (!is_tchar(s.p[i])) {
			return false;
		}
	}
	return true;
}

// Field values: visible characters, spaces, tabs and obs-text
static bool is_field_value(struct str s) {
	for (size_t i = 0; i < s.len; i++) {
		unsigned char c = (unsigned char)s.p[i];
		if ((c < 0x20 && c != '\t') || c == 0x7f) {
			return false;
		}
	}
	return true;
}

static struct str trim_ows(struct str s) {
	while (s.len > 0 && (s.p[0] == ' ' || s.p[0] == '\t')) {
		s = str_slice(s, 1, s.len);
	}
	while (s.len > 0 && (s.p[s.len - 1] == ' ' || s.p[s.len - 1] == '\t')) {
		s.len--;
	}
	return s;
}

// The next line of the head (LF or CRLF terminated); false if incomplete
static bool next_line(struct str in, size_t *pos, struct str *line) {
	struct str rest = str_slice(in, *pos, in.len);
	ptrdiff_t nl = str_index(rest, '\n');
	if (nl < 0) {
		return false;
	}
	*line = str_slice(rest, 0, (size_t)nl);
	if (line->len > 0 && line->p[line->len - 1] == '\r') {
		line->len--;
	}
	*pos += (size_t)nl + 1;
	return true;
}

static bool parse_request_line(struct str line, struct http_request *req) {
	struct str method, rest, target, version;
	if (!str_split_once(line, ' ', &method, &rest) ||
	    !str_split_once(rest, ' ', &target, &version) || !is_token(method) || target.len == 0) {
		return false;
	}
	for (size_t i = 0; i < target.len; i++) {
		if ((unsigned char)target.p[i] <= 0x20 || target.p[i] == 0x7f) {
			return false;
		}
	}
	if (version.len != 8 || !str_starts_with(version, STR_LIT("HTTP/1.")) ||
	    version.p[7] < '0' || version.p[7] > '9') {
		return false;
	}
	req->method = str_dup(method);
	req->target = str_dup(target);
	req->minor = (unsigned)(version.p[7] - '0');
	struct str path = str_from(req->target), query;
	(void)str_split_once(path, '?', &path, &query);
	req->path = path;
	return true;
}

static bool parse_header(struct str line, struct http_request *req) {
	struct str name, value;
	// A line starting with whitespace continues the previous one
	// (obs-fold), which is rejected like hyper does
	if (!str_split_once(line, ':', &name, &value) || !is_token(name)) {
		return false;
	}
	value = trim_ows(value);
	if (!is_field_value(value)) {
		return false;
	}
	vec_push(&req->headers, ((struct http_header){str_dup(name), str_dup(value)}));
	return true;
}

// Body framing and persistence (RFC 9112 sections 6 and 9.3)
static bool parse_framing(struct http_request *req) {
	bool has_length = false;
	for (size_t i = 0; i < req->headers.len; i++) {
		const struct http_header *h = &req->headers.data[i];
		if (strcasecmp(h->name, "Transfer-Encoding") == 0) {
			req->chunked = true;
		} else if (strcasecmp(h->name, "Content-Length") == 0) {
			unsigned long long n;
			struct str v = str_from(h->value);
			if (v.len == 0 || v.p[0] == '+' || !str_parse_u64(v, &n) ||
			    (has_length && n != req->content_length)) {
				return false;
			}
			req->content_length = (size_t)n;
			has_length = true;
		}
	}
	if (req->chunked && has_length) {
		return false;
	}
	if (req->minor == 0) {
		req->keep_alive = http_header_has_token(req, "Connection", "keep-alive");
	} else {
		req->keep_alive = !http_header_has_token(req, "Connection", "close");
	}
	return true;
}

enum http_parse_result http_parse_request(struct str in, struct http_request *req,
					  size_t *consumed) {
	*req = (struct http_request){0};
	*consumed = 0;
	size_t pos = 0;
	struct str line;
	// Empty lines before the request line are ignored (RFC 9112 2.2)
	do {
		if (!next_line(in, &pos, &line)) {
			return in.len > HTTP_MAX_HEADER_BYTES ? HTTP_HEADERS_TOO_LARGE
							      : HTTP_NEED_MORE;
		}
	} while (line.len == 0 && pos <= 8);
	if (!parse_request_line(line, req)) {
		http_request_free(req);
		return HTTP_BAD_REQUEST;
	}
	for (;;) {
		if (!next_line(in, &pos, &line)) {
			http_request_free(req);
			return in.len > HTTP_MAX_HEADER_BYTES ? HTTP_HEADERS_TOO_LARGE
							      : HTTP_NEED_MORE;
		}
		if (pos > HTTP_MAX_HEADER_BYTES || req->headers.len >= HTTP_MAX_HEADERS) {
			http_request_free(req);
			return HTTP_HEADERS_TOO_LARGE;
		}
		if (line.len == 0) {
			break;
		}
		if (!parse_header(line, req)) {
			http_request_free(req);
			return HTTP_BAD_REQUEST;
		}
	}
	if (!parse_framing(req)) {
		http_request_free(req);
		return HTTP_BAD_REQUEST;
	}
	*consumed = pos;
	return HTTP_REQUEST;
}

const char *http_header(const struct http_request *req, const char *name) {
	for (size_t i = 0; i < req->headers.len; i++) {
		if (strcasecmp(req->headers.data[i].name, name) == 0) {
			return req->headers.data[i].value;
		}
	}
	return nullptr;
}

struct header_values http_header_values(const struct http_request *req, const char *name) {
	struct str *v = nullptr;
	size_t n = 0;
	for (size_t i = 0; i < req->headers.len; i++) {
		if (strcasecmp(req->headers.data[i].name, name) == 0) {
			v = xreallocarray(v, n + 1, sizeof(*v));
			v[n++] = str_from(req->headers.data[i].value);
		}
	}
	return (struct header_values){.v = v, .n = n};
}

void http_header_values_free(struct header_values hv) {
	xfree((void *)(uintptr_t)hv.v);
}

bool http_header_has_token(const struct http_request *req, const char *name, const char *token) {
	for (size_t i = 0; i < req->headers.len; i++) {
		if (strcasecmp(req->headers.data[i].name, name) != 0) {
			continue;
		}
		struct str rest = str_from(req->headers.data[i].value), field;
		while (str_next_field(&rest, ',', &field)) {
			if (str_eq_ignore_ascii_case(str_trim_ascii(field), str_from(token))) {
				return true;
			}
		}
	}
	return false;
}
