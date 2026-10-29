#include "http/response.h"

#include <strings.h>

#include "base/alloc.h"

void http_response_free(struct http_response *resp) {
	xfree(resp->reason);
	for (size_t i = 0; i < resp->headers.len; i++) {
		xfree(resp->headers.data[i].name);
		xfree(resp->headers.data[i].value);
	}
	vec_free(&resp->headers);
	strbuf_free(&resp->body);
	*resp = (struct http_response){0};
}

const char *http_response_header(const struct http_response *resp, const char *name) {
	for (size_t i = 0; i < resp->headers.len; i++) {
		if (strcasecmp(resp->headers.data[i].name, name) == 0) {
			return resp->headers.data[i].value;
		}
	}
	return nullptr;
}

static const char *canonical_reason(int status) {
	static const struct {
		int status;
		const char *reason;
	} reasons[] = {
		{100, "Continue"},
		{101, "Switching Protocols"},
		{200, "OK"},
		{201, "Created"},
		{202, "Accepted"},
		{204, "No Content"},
		{301, "Moved Permanently"},
		{302, "Found"},
		{303, "See Other"},
		{304, "Not Modified"},
		{307, "Temporary Redirect"},
		{308, "Permanent Redirect"},
		{400, "Bad Request"},
		{401, "Unauthorized"},
		{403, "Forbidden"},
		{404, "Not Found"},
		{405, "Method Not Allowed"},
		{408, "Request Timeout"},
		{410, "Gone"},
		{413, "Payload Too Large"},
		{415, "Unsupported Media Type"},
		{429, "Too Many Requests"},
		{500, "Internal Server Error"},
		{501, "Not Implemented"},
		{502, "Bad Gateway"},
		{503, "Service Unavailable"},
		{504, "Gateway Timeout"},
	};
	for (size_t i = 0; i < sizeof(reasons) / sizeof(reasons[0]); i++) {
		if (reasons[i].status == status) {
			return reasons[i].reason;
		}
	}
	return "<unknown status code>";
}

void http_status_text(struct strbuf *out, int status) {
	strbuf_printf(out, "%d %s", status, canonical_reason(status));
}
