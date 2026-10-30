// HTTP/1.1 client for authentication backends and Web Push (soju-rs used
// reqwest): one connection per request, TLS with the system trust store
// and ALPN http/1.1, a timeout covering the whole exchange, and redirects
// followed like reqwest's default policy (at most 10; 301/302 turn POST
// into GET, 303 any method; credentials are dropped when the origin
// changes).
//
// Requests run on the calling thread's loop, and complete there. A request
// in flight holds the loop alive; http_client_cancel_all() ends those of
// the current loop (shutdown).
#pragma once

#include <stdint.h>
#include <uv.h>

#include "base/err.h"
#include "base/log.h"
#include "base/str.h"
#include "base/vec.h"
#include "http/response.h"

static constexpr char HTTP_INTERNAL_IP_ERROR[] = "cannot connect to internal IP address";

struct http_client_request {
	const char *method;
	const char *url;
	strvec headers; // "Name: value"
	struct strbuf body;
	uint64_t timeout_ms;
	// Refuse internal IP addresses: in URLs (redirects too) and in DNS
	// results (Web Push)
	bool external_only;
	unsigned max_redirects;
	const struct logger *log; // for connection errors; may be nullptr
};

void http_client_request_init(struct http_client_request *r, const char *method, const char *url);
void http_client_request_free(struct http_client_request *r);
[[gnu::format(printf, 3, 4)]] void
http_client_request_header(struct http_client_request *r, const char *name, const char *fmt, ...);

// resp is nullptr on error. Both are only valid during the call.
typedef void (*http_client_done_fn)(void *data, const struct http_response *resp,
				    const struct err *err);

// curl_global_init, once (cleaned up at exit)
void http_client_global_init(void);
// Final process teardown, after every HTTP request/worker has stopped and
// before OPENSSL_cleanup. Idempotent; no HTTP operations may follow it.
void http_client_global_cleanup(void);
// Sends the request (taken: req is reset) on the current loop
void http_client_send(uv_loop_t *loop, struct http_client_request *req, http_client_done_fn done,
		      void *data);
// Fails every request of the current thread's loop with "operation
// cancelled"
void http_client_cancel_all(void);
// Requests in flight on the current thread
[[nodiscard]] size_t http_client_pending(void);

// Whether an IP address is internal: loopback, unspecified, multicast,
// private, link-local (IPv4-mapped IPv6 addresses as IPv4)
[[nodiscard]] bool http_is_internal_ip(const struct sockaddr *sa);
// Fails if the URL's host is an internal IP address, parsed like a URL
// parser would (IPv4 numbers such as "2130706433" or "127.1" included)
[[nodiscard]] bool http_check_external_url(const char *url, struct err **err);
