// HTTP responses received by the HTTP client
#pragma once

#include "http/request.h"

// Bodies are small (JSON documents): bigger ones are refused
static constexpr size_t HTTP_MAX_RESPONSE_BODY = (size_t)1 << 20U;

struct http_response {
	int status;
	char *reason; // as sent
	VEC(struct http_header) headers; // of the final response
	struct strbuf body;
};

void http_response_free(struct http_response *resp);
// The first value of a header (case-insensitive name), or nullptr
[[nodiscard]] const char *http_response_header(const struct http_response *resp, const char *name);
// "404 Not Found": the status code with its canonical reason, like Rust's
// http::StatusCode Display
void http_status_text(struct strbuf *out, int status);
