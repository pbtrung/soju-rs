#include <stdatomic.h>
// The HTTP client on libcurl's multi interface, driven by the loop: one
// multi handle per loop thread, created with its first request and freed
// once it has none left, so an idle loop holds no handle
#include "http/client.h"

#include <arpa/inet.h>
#include <curl/curl.h>
#include <pthread.h>
#include <stdlib.h>

#include "base/alloc.h"
#include "base/ref.h"
#include "net/url.h"

void http_client_request_init(struct http_client_request *r, const char *method, const char *url) {
	*r = (struct http_client_request){
		.method = method, .url = url, .timeout_ms = 30000, .max_redirects = 10};
}

void http_client_request_free(struct http_client_request *r) {
	strvec_free(&r->headers);
	strbuf_free(&r->body);
}

void http_client_request_header(struct http_client_request *r, const char *name, const char *fmt,
				...) {
	struct strbuf sb = STRBUF_INIT;
	strbuf_printf(&sb, "%s: ", name);
	va_list ap;
	va_start(ap, fmt);
	strbuf_vprintf(&sb, fmt, ap);
	va_end(ap);
	strvec_push_take(&r->headers, strbuf_take(&sb));
}

struct call;

// The multi handle of a loop thread
struct curl_loop {
	struct ref_node ref;
	CURLM *multi;
	uv_loop_t *loop;
	uv_timer_t timer;
	struct call *calls; // in flight
};

// A socket curl wants polled
struct curl_sock {
	uv_poll_t poll;
	curl_socket_t fd;
	struct curl_loop *cl;
};

struct call {
	struct ref_node ref;
	struct call *prev, *next;
	struct curl_loop *cl;
	CURL *easy;
	struct curl_slist *headers;
	struct strbuf body; // request body
	char *url;
	http_client_done_fn done;
	void *data;
	bool external_only;
	bool refused_internal; // a connection to an internal address was refused
	struct http_response resp;
	char errbuf[CURL_ERROR_SIZE];
};

static thread_local struct curl_loop *current;
static pthread_once_t global_once = PTHREAD_ONCE_INIT;
static atomic_bool global_ready;

void http_client_global_cleanup(void) {
	if (atomic_exchange(&global_ready, false)) {
		curl_global_cleanup();
	}
}

static void global_init(void) {
	if (curl_global_init(CURL_GLOBAL_DEFAULT) != CURLE_OK) {
		abort();
	}
	atomic_store(&global_ready, true);
	atexit(http_client_global_cleanup);
}

static void timer_closed(uv_handle_t *h) {
	struct curl_loop *cl = h->data;
	ref_untrack(&cl->ref);
	xfree(cl);
}

// Frees the loop's multi handle once its last call is done
static void release_loop(struct curl_loop *cl) {
	if (cl->calls != nullptr) {
		return;
	}
	if (current == cl) {
		current = nullptr;
	}
	curl_multi_cleanup(cl->multi);
	uv_close((uv_handle_t *)&cl->timer, timer_closed);
}

static void call_free(struct call *c) {
	curl_easy_cleanup(c->easy);
	curl_slist_free_all(c->headers);
	strbuf_free(&c->body);
	xfree(c->url);
	http_response_free(&c->resp);
	ref_untrack(&c->ref);
	xfree(c);
}

// Removes the call from its multi handle and completes it: with the
// response, or with err
static void complete(struct call *c, const char *err) {
	curl_multi_remove_handle(c->cl->multi, c->easy);
	if (c->prev != nullptr) {
		c->prev->next = c->next;
	} else {
		c->cl->calls = c->next;
	}
	if (c->next != nullptr) {
		c->next->prev = c->prev;
	}
	if (err == nullptr) {
		long status = 0;
		curl_easy_getinfo(c->easy, CURLINFO_RESPONSE_CODE, &status);
		c->resp.status = (int)status;
		if (c->resp.reason == nullptr) {
			c->resp.reason = xstrdup("");
		}
		c->done(c->data, &c->resp, nullptr);
	} else {
		AUTOFREE char *msg =
			xasprintf("error sending request for url (%s): %s", c->url, err);
		struct err e = {.msg = msg};
		c->done(c->data, nullptr, &e);
	}
	call_free(c);
}

// A failed transfer, in reqwest's words where they differ
static void complete_failed(struct call *c, CURLcode code) {
	switch (code) {
	case CURLE_OPERATION_TIMEDOUT:
		complete(c, "operation timed out");
		return;
	case CURLE_TOO_MANY_REDIRECTS:
		complete(c, "stopped after 10 redirects");
		return;
	case CURLE_COULDNT_CONNECT:
		if (c->refused_internal) {
			complete(c, HTTP_INTERNAL_IP_ERROR);
			return;
		}
		break;
	default:
		break;
	}
	AUTOFREE char *msg =
		c->errbuf[0] != '\0' ? xstrdup(c->errbuf) : xstrdup(curl_easy_strerror(code));
	complete(c, msg);
}

// Completes the finished transfers
static void check_done(struct curl_loop *cl) {
	CURLMsg *m;
	int left;
	while ((m = curl_multi_info_read(cl->multi, &left)) != nullptr) {
		if (m->msg != CURLMSG_DONE) {
			continue;
		}
		struct call *c = nullptr;
		curl_easy_getinfo(m->easy_handle, CURLINFO_PRIVATE, &c);
		CURLcode code = m->data.result;
		if (code == CURLE_OK) {
			complete(c, nullptr);
		} else {
			complete_failed(c, code);
		}
	}
}

static void socket_action(struct curl_loop *cl, curl_socket_t fd, int flags) {
	int running;
	curl_multi_socket_action(cl->multi, fd, flags, &running);
	check_done(cl);
	release_loop(cl);
}

static void on_poll(uv_poll_t *p, int status, int events) {
	struct curl_sock *s = p->data;
	int flags = 0;
	if (status < 0) {
		flags = CURL_CSELECT_ERR;
	} else {
		bool in = ((unsigned)events & (unsigned)UV_READABLE) != 0;
		bool out = ((unsigned)events & (unsigned)UV_WRITABLE) != 0;
		flags = in && out ? CURL_CSELECT_IN + CURL_CSELECT_OUT
			: in      ? CURL_CSELECT_IN
			: out     ? CURL_CSELECT_OUT
				  : 0;
	}
	socket_action(s->cl, s->fd, flags);
}

static void on_timer(uv_timer_t *t) {
	socket_action(t->data, CURL_SOCKET_TIMEOUT, 0);
}

static void poll_closed(uv_handle_t *h) {
	xfree(h->data);
}

static int socket_cb(CURL *easy, curl_socket_t fd, int what, void *userp, void *socketp) {
	(void)easy;
	struct curl_loop *cl = userp;
	struct curl_sock *s = socketp;
	if (what == CURL_POLL_REMOVE) {
		if (s != nullptr) {
			uv_poll_stop(&s->poll);
			uv_close((uv_handle_t *)&s->poll, poll_closed);
			curl_multi_assign(cl->multi, fd, nullptr);
		}
		return 0;
	}
	if (s == nullptr) {
		s = xcalloc(1, sizeof(*s));
		s->fd = fd;
		s->cl = cl;
		if (uv_poll_init_socket(cl->loop, &s->poll, fd) != 0) {
			xfree(s);
			return -1;
		}
		s->poll.data = s;
		curl_multi_assign(cl->multi, fd, s);
	}
	// CURL_POLL_INOUT is CURL_POLL_IN | CURL_POLL_OUT
	int events = what == CURL_POLL_INOUT ? UV_READABLE + UV_WRITABLE
		     : what == CURL_POLL_IN  ? UV_READABLE
		     : what == CURL_POLL_OUT ? UV_WRITABLE
					     : 0;
	uv_poll_start(&s->poll, events, on_poll);
	return 0;
}

static int timer_cb(CURLM *multi, long timeout_ms, void *userp) {
	(void)multi;
	struct curl_loop *cl = userp;
	if (timeout_ms < 0) {
		uv_timer_stop(&cl->timer);
	} else {
		uv_timer_start(&cl->timer, on_timer, (uint64_t)timeout_ms, 0);
	}
	return 0;
}

void http_client_global_init(void) {
	pthread_once(&global_once, global_init);
}

static struct curl_loop *get_loop(uv_loop_t *loop) {
	if (current != nullptr) {
		return current;
	}
	http_client_global_init();
	struct curl_loop *cl = xcalloc(1, sizeof(*cl));
	REF_TRACK(&cl->ref, "curl_loop");
	cl->loop = loop;
	cl->multi = curl_multi_init();
	if (cl->multi == nullptr) {
		die_oom();
	}
	curl_multi_setopt(cl->multi, CURLMOPT_SOCKETFUNCTION, socket_cb);
	curl_multi_setopt(cl->multi, CURLMOPT_SOCKETDATA, cl);
	curl_multi_setopt(cl->multi, CURLMOPT_TIMERFUNCTION, timer_cb);
	curl_multi_setopt(cl->multi, CURLMOPT_TIMERDATA, cl);
	uv_timer_init(loop, &cl->timer);
	cl->timer.data = cl;
	current = cl;
	return cl;
}

static bool internal_v4(uint32_t a) {
	unsigned o1 = a >> 24U, o2 = (a >> 16U) & 0xffU, o3 = (a >> 8U) & 0xffU;
	return o1 == 0 || // "this network"
	       o1 == 127 || (o1 >= 224 && o1 <= 239) || // loopback, multicast
	       o1 == 10 || (o1 == 172 && (o2 & 0xf0U) == 16) || (o1 == 192 && o2 == 168) ||
	       (o1 == 169 && o2 == 254) || // link-local
	       (o1 == 100 && (o2 & 0xc0U) == 64) || // shared space (CGNAT, metadata)
	       (o1 == 192 && o2 == 0 && o3 == 0) || // IETF protocol assignments
	       (o1 == 198 && (o2 & 0xfeU) == 18) || // benchmarking
	       o1 >= 240; // reserved, broadcast
}

// Not globally reachable, or a gateway to such an address (an IPv6 address
// embedding an IPv4 one)
bool http_is_internal_ip(const struct sockaddr *sa) {
	if (sa->sa_family == AF_INET) {
		return internal_v4(ntohl(((const struct sockaddr_in *)sa)->sin_addr.s_addr));
	}
	if (sa->sa_family != AF_INET6) {
		return false;
	}
	const uint8_t *b = ((const struct sockaddr_in6 *)sa)->sin6_addr.s6_addr;
	uint32_t low = ((uint32_t)b[12] << 24U) | ((uint32_t)b[13] << 16U) |
		       ((uint32_t)b[14] << 8U) | b[15];
	static const uint8_t zero[16] = {0};
	static const uint8_t mapped[12] = {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff};
	static const uint8_t nat64[12] = {0, 0x64, 0xff, 0x9b};
	static const uint8_t nat64_local[6] = {0, 0x64, 0xff, 0x9b, 0, 1};
	if (memcmp(b, mapped, 12) == 0) {
		return internal_v4(low);
	}
	bool loopback = memcmp(b, zero, 15) == 0 && b[15] == 1;
	return loopback || memcmp(b, zero, 16) == 0 || b[0] == 0xff || // multicast
	       (b[0] & 0xfeU) == 0xfc || // unique local
	       (b[0] == 0xfe && (b[1] & 0xc0U) == 0x80) || // link-local
	       (b[0] == 0xfe && (b[1] & 0xc0U) == 0xc0) || // site-local
	       (memcmp(b, zero, 12) == 0 && internal_v4(low)) || // IPv4-compatible
	       (memcmp(b, nat64, 12) == 0 && internal_v4(low)) || // NAT64
	       memcmp(b, nat64_local, 6) == 0 || // local-use NAT64
	       (b[0] == 0x20 && b[1] == 0x02 && // 6to4
		internal_v4(((uint32_t)b[2] << 24U) | ((uint32_t)b[3] << 16U) |
			    ((uint32_t)b[4] << 8U) | b[5]));
}

bool http_check_external_url(const char *url, struct err **err) {
	struct url u;
	if (!url_parse(str_from(url), &u, err)) {
		return false;
	}
	char *host = nullptr, *port = nullptr;
	if (!split_host_port(str_from(u.host), &host, &port, nullptr)) {
		host = xstrdup(u.host);
		size_t n = strlen(host);
		if (n > 2 && host[0] == '[' && host[n - 1] == ']') {
			memmove(host, host + 1, n - 2);
			host[n - 2] = '\0';
		}
	}
	xfree(port);
	url_free(&u);
	// IP literals in any form a URL parser accepts: IPv6, and IPv4 as
	// inet_aton reads it ("127.1", "0x7f.1", "2130706433")
	struct sockaddr_storage ss = {0};
	struct sockaddr_in *in = (struct sockaddr_in *)&ss;
	struct sockaddr_in6 *in6 = (struct sockaddr_in6 *)&ss;
	bool ip = false;
	if (inet_pton(AF_INET6, host, &in6->sin6_addr) == 1) {
		in6->sin6_family = AF_INET6;
		ip = true;
	} else if (inet_aton(host, &in->sin_addr) != 0) {
		in->sin_family = AF_INET;
		ip = true;
	}
	xfree(host);
	if (ip && http_is_internal_ip((const struct sockaddr *)&ss)) {
		return err_set(err, 0, "%s", HTTP_INTERNAL_IP_ERROR);
	}
	return true;
}

// Every connection of a request, redirects included: external-only
// requests never reach an internal address, whatever DNS says
static curl_socket_t open_socket(void *data, curlsocktype purpose, struct curl_sockaddr *addr) {
	(void)purpose;
	struct call *c = data;
	if (c->external_only && http_is_internal_ip(&addr->addr)) {
		c->refused_internal = true;
		return CURL_SOCKET_BAD;
	}
	return socket(addr->family, (int)((unsigned)addr->socktype | (unsigned)SOCK_CLOEXEC),
		      addr->protocol);
}

static size_t on_body(char *p, size_t size, size_t n, void *data) {
	struct call *c = data;
	size_t len = size * n;
	if (c->resp.body.len + len > HTTP_MAX_RESPONSE_BODY) {
		return 0; // CURLE_WRITE_ERROR
	}
	strbuf_append(&c->resp.body, p, len);
	return len;
}

// Header lines; a status line starts a new response (after a redirect or
// an interim response): only the last one's headers are kept
static size_t on_header(char *p, size_t size, size_t n, void *data) {
	struct call *c = data;
	size_t len = size * n;
	struct str line = str_trim_ascii_end(str_new(p, len));
	if (str_starts_with(line, STR_LIT("HTTP/"))) {
		http_response_free(&c->resp);
		struct str version, rest, code, reason = STR_EMPTY;
		if (str_split_once(line, ' ', &version, &rest)) {
			(void)str_split_once(rest, ' ', &code, &reason);
		}
		c->resp.reason = str_dup(reason);
		return len;
	}
	struct str name, value;
	if (str_split_once(line, ':', &name, &value)) {
		vec_push(&c->resp.headers,
			 ((struct http_header){str_dup(name), str_dup(str_trim_ascii(value))}));
	}
	return len;
}

static void setup(struct call *c, const struct http_client_request *req) {
	CURL *e = c->easy;
	curl_easy_setopt(e, CURLOPT_PRIVATE, c);
	curl_easy_setopt(e, CURLOPT_URL, c->url);
	curl_easy_setopt(e, CURLOPT_ERRORBUFFER, c->errbuf);
	curl_easy_setopt(e, CURLOPT_NOSIGNAL, 1L);
	curl_easy_setopt(e, CURLOPT_PROTOCOLS_STR, "http,https");
	// External-only requests (Web Push) never downgrade to plain HTTP
	curl_easy_setopt(e, CURLOPT_REDIR_PROTOCOLS_STR, c->external_only ? "https" : "http,https");
	curl_easy_setopt(e, CURLOPT_HTTP_VERSION, (long)CURL_HTTP_VERSION_1_1);
	curl_easy_setopt(e, CURLOPT_TIMEOUT_MS, (long)req->timeout_ms);
	// Like reqwest's default policy: credentials are only sent to the
	// original host (curl's default without CURLOPT_UNRESTRICTED_AUTH),
	// 301/302/303 turn a POST into a GET (without CURLOPT_POSTREDIR)
	curl_easy_setopt(e, CURLOPT_FOLLOWLOCATION, req->max_redirects > 0 ? 1L : 0L);
	curl_easy_setopt(e, CURLOPT_MAXREDIRS, (long)req->max_redirects);
	curl_easy_setopt(e, CURLOPT_WRITEFUNCTION, on_body);
	curl_easy_setopt(e, CURLOPT_WRITEDATA, c);
	curl_easy_setopt(e, CURLOPT_HEADERFUNCTION, on_header);
	curl_easy_setopt(e, CURLOPT_HEADERDATA, c);
	curl_easy_setopt(e, CURLOPT_OPENSOCKETFUNCTION, open_socket);
	curl_easy_setopt(e, CURLOPT_OPENSOCKETDATA, c);
	if (c->external_only) {
		// Like reqwest's no_proxy(): the address checks need a direct
		// connection
		curl_easy_setopt(e, CURLOPT_PROXY, "");
	}
	for (size_t i = 0; i < req->headers.len; i++) {
		c->headers = curl_slist_append(c->headers, req->headers.data[i]);
	}
	// No "Expect: 100-continue" round trip for bodies
	c->headers = curl_slist_append(c->headers, "Expect:");
	curl_easy_setopt(e, CURLOPT_HTTPHEADER, c->headers);
	if (cstr_eq(req->method, "GET")) {
		curl_easy_setopt(e, CURLOPT_HTTPGET, 1L);
	} else if (cstr_eq(req->method, "HEAD")) {
		curl_easy_setopt(e, CURLOPT_NOBODY, 1L);
	} else {
		if (!cstr_eq(req->method, "POST")) {
			curl_easy_setopt(e, CURLOPT_CUSTOMREQUEST, req->method);
		}
		curl_easy_setopt(e, CURLOPT_POSTFIELDSIZE_LARGE, (curl_off_t)c->body.len);
		curl_easy_setopt(e, CURLOPT_POSTFIELDS, c->body.len > 0 ? c->body.buf : "");
	}
}

void http_client_send(uv_loop_t *loop, struct http_client_request *req, http_client_done_fn done,
		      void *data) {
	struct curl_loop *cl = get_loop(loop);
	struct call *c = xcalloc(1, sizeof(*c));
	REF_TRACK(&c->ref, "http_call");
	c->cl = cl;
	c->done = done;
	c->data = data;
	c->url = xstrdup(req->url);
	c->external_only = req->external_only;
	c->body = req->body;
	req->body = STRBUF_INIT;
	c->easy = curl_easy_init();
	if (c->easy == nullptr) {
		die_oom();
	}
	setup(c, req);
	c->next = cl->calls;
	if (cl->calls != nullptr) {
		cl->calls->prev = c;
	}
	cl->calls = c;
	// The transfer starts from the loop (curl sets a 0 ms timer): the
	// completion never runs inside the caller
	CURLMcode rc = curl_multi_add_handle(cl->multi, c->easy);
	if (rc != CURLM_OK) {
		complete(c, curl_multi_strerror(rc));
		release_loop(cl);
	}
}

void http_client_cancel_all(void) {
	struct curl_loop *cl = current;
	if (cl == nullptr) {
		return;
	}
	while (cl->calls != nullptr) {
		complete(cl->calls, "operation cancelled");
	}
	release_loop(cl);
}

size_t http_client_pending(void) {
	size_t n = 0;
	for (struct call *c = current != nullptr ? current->calls : nullptr; c != nullptr;
	     c = c->next) {
		n++;
	}
	return n;
}
