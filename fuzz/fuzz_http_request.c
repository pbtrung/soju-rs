// HTTP request heads (PLAN.md 2.3 L5): no crash, the limits hold, and a
// parsed head is parsed the same way when more bytes follow it
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "base/alloc.h"
#include "http/request.h"

// NOLINTNEXTLINE(misc-use-internal-linkage)
int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size);

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size) {
	struct str in = str_new((const char *)data, size);
	struct http_request req;
	size_t used = 0;
	enum http_parse_result r = http_parse_request(in, &req, &used);
	if (r != HTTP_REQUEST) {
		return 0;
	}
	if (used == 0 || used > size || used > HTTP_MAX_HEADER_BYTES ||
	    req.headers.len > HTTP_MAX_HEADERS || req.path.len > strlen(req.target)) {
		abort();
	}
	// The head alone parses to the same request
	struct http_request again;
	size_t used2 = 0;
	if (http_parse_request(str_new(in.p, used), &again, &used2) != HTTP_REQUEST ||
	    used2 != used || strcmp(again.method, req.method) != 0 ||
	    strcmp(again.target, req.target) != 0 || again.headers.len != req.headers.len ||
	    again.content_length != req.content_length || again.keep_alive != req.keep_alive) {
		abort();
	}
	(void)http_header(&req, "Host");
	struct header_values hv = http_header_values(&req, "X-Forwarded-For");
	http_header_values_free(hv);
	http_request_free(&again);
	http_request_free(&req);
	return 0;
}
