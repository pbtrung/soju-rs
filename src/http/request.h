// HTTP/1.1 requests (RFC 9112) read by the HTTP listeners: the request
// line and header section, and the framing of the body, which is ignored
// (soju-rs used hyper)
#pragma once

#include <stddef.h>

#include "base/str.h"
#include "base/vec.h"
#include "http/forwarded.h"

// Limits on the header section, like hyper's (100 headers)
static constexpr size_t HTTP_MAX_HEADER_BYTES = 65536;
static constexpr size_t HTTP_MAX_HEADERS = 100;

struct http_header {
	char *name; // as received
	char *value; // without surrounding whitespace
};

struct http_request {
	char *method;
	char *target; // request target as received
	struct str path; // target without the query (points into target)
	unsigned minor; // HTTP/1.minor
	VEC(struct http_header) headers;
	bool keep_alive; // the connection may serve another request
	bool chunked; // Transfer-Encoding: the body length is unknown
	size_t content_length;
};

enum http_parse_result {
	HTTP_NEED_MORE,
	HTTP_REQUEST_READY, // a request was parsed
	HTTP_BAD_REQUEST, // 400
	HTTP_HEADERS_TOO_LARGE, // 431
};

// Parses a request's head from the start of in; *consumed is the length of
// the head when a request is parsed
[[nodiscard]] enum http_parse_result http_parse_request(struct str in, struct http_request *req,
							size_t *consumed);
void http_request_free(struct http_request *req);

// The first value of a header (case-insensitive name), or nullptr
[[nodiscard]] const char *http_header(const struct http_request *req, const char *name);
// All the values of a header, in order, pointing into req (freed with
// http_header_values_free)
[[nodiscard]] struct header_values http_header_values(const struct http_request *req,
						      const char *name);
void http_header_values_free(struct header_values hv);
// Whether a comma-separated header (Connection, Upgrade) has a token,
// ignoring case
[[nodiscard]] bool http_header_has_token(const struct http_request *req, const char *name,
					 const char *token);
