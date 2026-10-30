#include "http/request.h"

#include <llhttp.h>
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

// llhttp owns HTTP syntax and message framing. This adapter copies header
// spans into the application's request representation and pauses exactly at
// the end of the head, leaving bodies and upgrade bytes for the caller.
struct request_parser {
	struct http_request *req;
	struct strbuf target, name, value;
	bool too_many_headers;
};

static int on_target(llhttp_t *parser, const char *at, size_t length) {
	struct request_parser *p = parser->data;
	strbuf_append(&p->target, at, length);
	return 0;
}

static int on_field(llhttp_t *parser, const char *at, size_t length) {
	struct request_parser *p = parser->data;
	strbuf_append(&p->name, at, length);
	return 0;
}

static int on_value(llhttp_t *parser, const char *at, size_t length) {
	struct request_parser *p = parser->data;
	// Obsolete folded fields are rejected, matching hyper.
	if (memchr(at, '\r', length) != nullptr || memchr(at, '\n', length) != nullptr) {
		return HPE_USER;
	}
	strbuf_append(&p->value, at, length);
	return 0;
}

static int on_field_complete(llhttp_t *parser) {
	struct request_parser *p = parser->data;
	if (p->req->headers.len >= HTTP_MAX_HEADERS) {
		p->too_many_headers = true;
		return HPE_USER;
	}
	struct http_header h = {
		.name = str_dup(strbuf_str(&p->name)),
		.value = str_dup(str_trim_ascii(strbuf_str(&p->value))),
	};
	vec_push(&p->req->headers, h);
	strbuf_reset(&p->name);
	strbuf_reset(&p->value);
	return 0;
}

static int on_head(llhttp_t *parser) {
	struct request_parser *p = parser->data;
	struct http_request *req = p->req;
	if (parser->http_major != 1) {
		return HPE_USER;
	}
	req->method = xstrdup(llhttp_method_name((llhttp_method_t)parser->method));
	req->target = str_dup(strbuf_str(&p->target));
	req->minor = parser->http_minor;
	req->path = str_from(req->target);
	struct str query;
	(void)str_split_once(req->path, '?', &req->path, &query);
	req->keep_alive = llhttp_should_keep_alive(parser) != 0;
	req->chunked = ((unsigned)parser->flags & (unsigned)F_CHUNKED) != 0;
	req->content_length = ((unsigned)parser->flags & (unsigned)F_CONTENT_LENGTH) != 0
				      ? (size_t)parser->content_length
				      : 0;
	return HPE_PAUSED;
}

enum http_parse_result http_parse_request(struct str in, struct http_request *req,
					  size_t *consumed) {
	*req = (struct http_request){0};
	*consumed = 0;
	struct request_parser p = {.req = req};
	llhttp_settings_t settings;
	llhttp_settings_init(&settings);
	settings.on_url = on_target;
	settings.on_header_field = on_field;
	settings.on_header_value = on_value;
	settings.on_header_value_complete = on_field_complete;
	settings.on_headers_complete = on_head;
	llhttp_t parser;
	llhttp_init(&parser, HTTP_REQUEST, &settings);
	parser.data = &p;
	// Hyper accepts LF-terminated heads. Normalize that spelling before
	// llhttp, retaining offsets so upgrade/body bytes remain untouched.
	AUTO_STRBUF struct strbuf normalized = STRBUF_INIT;
	size_t length = in.len > HTTP_MAX_HEADER_BYTES ? HTTP_MAX_HEADER_BYTES + 1 : in.len;
	for (size_t i = 0; i < length; i++) {
		if (in.p[i] == '\n' && (i == 0 || in.p[i - 1] != '\r')) {
			strbuf_append_char(&normalized, '\r');
		}
		strbuf_append_char(&normalized, in.p[i]);
	}
	const char *input = strbuf_cstr(&normalized);
	const char *space = memchr(input, ' ', normalized.len);
	bool extra_space =
		space != nullptr && (size_t)(space - input) + 1 < normalized.len && space[1] == ' ';
	llhttp_errno_t result =
		extra_space ? HPE_INVALID_URL : llhttp_execute(&parser, input, normalized.len);
	enum http_parse_result out;
	if (result == HPE_PAUSED) {
		size_t parsed = (size_t)(llhttp_get_error_pos(&parser) - input);
		size_t offset = 0;
		while (*consumed < length && offset < parsed) {
			size_t i = (*consumed)++;
			offset += in.p[i] == '\n' && (i == 0 || in.p[i - 1] != '\r') ? 2 : 1;
		}
		out = *consumed > HTTP_MAX_HEADER_BYTES ? HTTP_HEADERS_TOO_LARGE
							: HTTP_REQUEST_READY;
	} else if (p.too_many_headers || (result == HPE_OK && in.len > HTTP_MAX_HEADER_BYTES)) {
		out = HTTP_HEADERS_TOO_LARGE;
	} else {
		out = result == HPE_OK ? HTTP_NEED_MORE : HTTP_BAD_REQUEST;
	}
	strbuf_free(&p.target);
	strbuf_free(&p.name);
	strbuf_free(&p.value);
	if (out != HTTP_REQUEST_READY) {
		*consumed = 0;
		http_request_free(req);
	}
	return out;
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
