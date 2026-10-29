// A mock HTTP/1.1 server for client tests: a thread answering requests on
// 127.0.0.1 with a handler, one request per connection
#pragma once

#include <stdint.h>

#include "base/str.h"

// request: the head and body as received. The handler writes the full
// response (status line, headers, body) to response; an empty response
// keeps the connection open without answering (until the server stops).
typedef void (*http_mock_handler)(const struct strbuf *request, struct strbuf *response,
				  void *data);

struct http_mock;

[[nodiscard]] struct http_mock *http_mock_start(http_mock_handler h, void *data);
[[nodiscard]] uint16_t http_mock_port(const struct http_mock *m);
// The number of requests received so far
[[nodiscard]] unsigned http_mock_requests(struct http_mock *m);
void http_mock_stop(struct http_mock *m);
