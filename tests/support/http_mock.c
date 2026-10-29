#include "http_mock.h"

#include <arpa/inet.h>
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <unistd.h>

#include "base/alloc.h"
#include "testsupport.h"

struct http_mock {
	int listener;
	uint16_t port;
	pthread_t thread;
	atomic_bool stop;
	atomic_uint requests;
	http_mock_handler handler;
	void *data;
	int held[16]; // connections left unanswered
	size_t nheld;
};

// Reads a request: the head, then Content-Length bytes
static bool read_request(int fd, struct strbuf *req) {
	for (;;) {
		ptrdiff_t end = str_find(strbuf_str(req), STR_LIT("\r\n\r\n"));
		if (end >= 0 && req->buf != nullptr) {
			const char *cl = strcasestr(req->buf, "content-length:");
			size_t len = cl != nullptr && cl < req->buf + end
					     ? strtoul(cl + strlen("content-length:"), nullptr, 10)
					     : 0;
			if (req->len >= (size_t)end + 4 + len) {
				return true;
			}
		}
		struct pollfd p = {.fd = fd, .events = POLLIN};
		if (poll(&p, 1, 5000) != 1) {
			return false;
		}
		char buf[4096];
		ssize_t n = recv(fd, buf, sizeof(buf), 0);
		if (n <= 0) {
			return false;
		}
		strbuf_append(req, buf, (size_t)n);
	}
}

static void *serve(void *arg) {
	struct http_mock *m = arg;
	while (!atomic_load(&m->stop)) {
		struct pollfd p = {.fd = m->listener, .events = POLLIN};
		if (poll(&p, 1, 50) != 1) {
			continue;
		}
		int fd = accept(m->listener, nullptr, nullptr);
		if (fd < 0) {
			continue;
		}
		struct strbuf req = STRBUF_INIT, resp = STRBUF_INIT;
		if (read_request(fd, &req)) {
			atomic_fetch_add(&m->requests, 1);
			m->handler(&req, &resp, m->data);
		}
		if (resp.len == 0 && m->nheld < sizeof(m->held) / sizeof(m->held[0])) {
			m->held[m->nheld++] = fd;
		} else {
			size_t off = 0;
			while (off < resp.len) {
				ssize_t n = send(fd, resp.buf + off, resp.len - off, MSG_NOSIGNAL);
				if (n <= 0) {
					break;
				}
				off += (size_t)n;
			}
			close(fd);
		}
		strbuf_free(&req);
		strbuf_free(&resp);
	}
	return nullptr;
}

struct http_mock *http_mock_start(http_mock_handler h, void *data) {
	struct http_mock *m = xcalloc(1, sizeof(*m));
	m->handler = h;
	m->data = data;
	m->listener = socket(AF_INET, SOCK_STREAM, 0);
	if (m->listener < 0) {
		abort();
	}
	struct sockaddr_in addr = {.sin_family = AF_INET,
				   .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
	assert_int_equal(bind(m->listener, (struct sockaddr *)&addr, sizeof(addr)), 0);
	assert_int_equal(listen(m->listener, 16), 0);
	socklen_t len = sizeof(addr);
	assert_int_equal(getsockname(m->listener, (struct sockaddr *)&addr, &len), 0);
	m->port = ntohs(addr.sin_port);
	assert_int_equal(pthread_create(&m->thread, nullptr, serve, m), 0);
	return m;
}

uint16_t http_mock_port(const struct http_mock *m) {
	return m->port;
}

unsigned http_mock_requests(struct http_mock *m) {
	return atomic_load(&m->requests);
}

void http_mock_stop(struct http_mock *m) {
	atomic_store(&m->stop, true);
	pthread_join(m->thread, nullptr);
	for (size_t i = 0; i < m->nheld; i++) {
		close(m->held[i]);
	}
	close(m->listener);
	xfree(m);
}
