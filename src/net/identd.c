#include "net/identd.h"

#include <string.h>

#include "base/alloc.h"
#include "base/ref.h"
#include "base/str.h"
#include "base/utf8.h"
#include "net/proxyproto.h"
#include "net/url.h"

static constexpr uint64_t IDENTD_TIMEOUT_MS = 10000;
// We only need to read lines with two port numbers
static constexpr size_t MAX_QUERY = 512;

void identd_init(struct identd *id) {
	pthread_mutex_init(&id->mu, nullptr);
	strmap_init(&id->entries, xfree);
}

void identd_finish(struct identd *id) {
	strmap_free(&id->entries);
	pthread_mutex_destroy(&id->mu);
}

static bool split(const char *addr, char **host, uint16_t *port) {
	char *h, *p;
	if (!split_host_port(str_from(addr), &h, &p, nullptr)) {
		return false;
	}
	unsigned long long v;
	bool ok = str_parse_u64(str_from(p), &v) && v <= 65535;
	xfree(p);
	if (!ok) {
		xfree(h);
		return false;
	}
	*host = h;
	*port = (uint16_t)v;
	return true;
}

static char *key(const char *remote_host, uint16_t remote_port, uint16_t local_port) {
	return xasprintf("%s\n%u\n%u", remote_host, remote_port, local_port);
}

static char *key_of(const char *remote_addr, const char *local_addr) {
	char *rh, *lh;
	uint16_t rp, lp;
	if (!split(remote_addr, &rh, &rp)) {
		return nullptr;
	}
	if (!split(local_addr, &lh, &lp)) {
		xfree(rh);
		return nullptr;
	}
	xfree(lh);
	char *k = key(rh, rp, lp);
	xfree(rh);
	return k;
}

void identd_store(struct identd *id, const char *remote_addr, const char *local_addr,
		  const char *ident) {
	AUTOFREE char *k = key_of(remote_addr, local_addr);
	if (k != nullptr) {
		pthread_mutex_lock(&id->mu);
		strmap_set(&id->entries, k, xstrdup(ident));
		pthread_mutex_unlock(&id->mu);
	}
}

void identd_delete(struct identd *id, const char *remote_addr, const char *local_addr) {
	AUTOFREE char *k = key_of(remote_addr, local_addr);
	if (k != nullptr) {
		pthread_mutex_lock(&id->mu);
		strmap_remove(&id->entries, k);
		pthread_mutex_unlock(&id->mu);
	}
}

bool identd_parse_query(const char *l, uint16_t *local, uint16_t *remote) {
	struct str a, b;
	if (!str_split_once(str_from(l), ',', &a, &b)) {
		return false;
	}
	long long lv, rv;
	if (!str_parse_i64(utf8_trim(a), &lv) || !str_parse_i64(utf8_trim(b), &rv)) {
		return false;
	}
	if (lv <= 0 || rv <= 0 || lv > 65535 || rv > 65535) {
		return false;
	}
	*local = (uint16_t)lv;
	*remote = (uint16_t)rv;
	return true;
}

struct session {
	struct ref_node ref;
	struct identd *id;
	uv_stream_t *stream;
	uv_timer_t timer;
	char *remote_host;
	struct strbuf buf;
	bool done; // no more queries: close once written
	bool proxy_done;
	bool trust_proxy;
	unsigned pending; // handles not closed yet, writes in flight
	bool closing;
};

static void session_unref(struct session *s) {
	if (--s->pending == 0) {
		xfree(s->remote_host);
		strbuf_free(&s->buf);
		ref_untrack(&s->ref);
		xfree(s);
	}
}

static void on_closed(uv_handle_t *h) {
	struct session *s = h->data;
	if (h == (uv_handle_t *)s->stream) {
		xfree(s->stream);
	}
	session_unref(s);
}

static void session_close(struct session *s) {
	if (s->closing) {
		return;
	}
	s->closing = true;
	uv_read_stop(s->stream);
	uv_close((uv_handle_t *)&s->timer, on_closed);
	uv_close((uv_handle_t *)s->stream, on_closed);
}

struct write_req {
	uv_write_t req;
	struct session *s;
	char *data;
};

static void on_written(uv_write_t *req, int status) {
	struct write_req *w = (struct write_req *)req;
	struct session *s = w->s;
	xfree(w->data);
	xfree(w);
	if (status < 0 || s->done) {
		session_close(s);
	}
	session_unref(s);
}

static void reply(struct session *s, char *line) {
	struct write_req *w = xcalloc(1, sizeof(*w));
	w->s = s;
	w->data = line;
	uv_buf_t buf = uv_buf_init(line, (unsigned)strlen(line));
	s->pending++;
	if (uv_write(&w->req, s->stream, &buf, 1, on_written) != 0) {
		xfree(w->data);
		xfree(w);
		s->pending--;
		session_close(s);
	}
}

static void handle_line(struct session *s, struct str raw) {
	struct str l = raw;
	while (l.len > 0 && (l.p[l.len - 1] == '\r' || l.p[l.len - 1] == '\n')) {
		l.len--;
	}
	AUTOFREE char *line = str_dup(l);
	uint16_t local, remote;
	if (!identd_parse_query(line, &local, &remote)) {
		s->done = true;
		reply(s, xasprintf("%s : ERROR : INVALID-PORT\r\n", line));
		return;
	}
	AUTOFREE char *k = key(s->remote_host, remote, local);
	pthread_mutex_lock(&s->id->mu);
	const char *ident = strmap_get(&s->id->entries, k);
	char *copy = ident != nullptr ? xstrdup(ident) : nullptr;
	pthread_mutex_unlock(&s->id->mu);
	if (copy == nullptr) {
		s->done = true;
		reply(s, xasprintf("%s : ERROR : NO-USER\r\n", line));
		return;
	}
	// The "OTHER" operating system may be rejected by IRC servers, because
	// it may be used when the ident string isn't stable. Use "UNKNOWN" from
	// RFC 1340 instead.
	reply(s, xasprintf("%s : USERID : UNKNOWN : %s\r\n", line, copy));
	xfree(copy);
}

static void on_timeout(uv_timer_t *t) {
	session_close(t->data);
}

static void on_alloc(uv_handle_t *h, size_t suggested, uv_buf_t *buf) {
	(void)h;
	(void)suggested;
	buf->base = xmalloc(1024);
	buf->len = 1024;
}

static void on_read(uv_stream_t *stream, ssize_t nread, const uv_buf_t *buf) {
	struct session *s = stream->data;
	if (nread > 0 && !s->done) {
		strbuf_append(&s->buf, buf->base, (size_t)nread);
		if (!s->proxy_done) {
			struct proxy_header h = {0};
			size_t consumed = 0;
			switch (proxy_parse(strbuf_str(&s->buf), false, &h, &consumed, nullptr)) {
			case PROXY_NEED_MORE:
				xfree(buf->base);
				return;
			case PROXY_ERROR:
				session_close(s);
				xfree(buf->base);
				return;
			case PROXY_NONE:
				break;
			case PROXY_HEADER:
				if (s->trust_proxy && h.has_source) {
					struct strbuf ip = STRBUF_INIT;
					ipaddr_format(&ip, ipaddr_canonical(h.source_ip));
					xfree(s->remote_host);
					s->remote_host = strbuf_take(&ip);
				}
				memmove(s->buf.buf, s->buf.buf + consumed, s->buf.len - consumed);
				strbuf_truncate(&s->buf, s->buf.len - consumed);
				proxy_header_free(&h);
				break;
			}
			s->proxy_done = true;
		}
		while (!s->done && !s->closing) {
			const char *nl =
				s->buf.len > 0 ? memchr(s->buf.buf, '\n', s->buf.len) : nullptr;
			size_t n;
			if (nl != nullptr && (size_t)(nl - s->buf.buf) + 1 <= MAX_QUERY) {
				n = (size_t)(nl - s->buf.buf) + 1;
			} else if (s->buf.len >= MAX_QUERY) {
				// read_line on take(512): a partial line
				n = MAX_QUERY;
			} else {
				break;
			}
			struct str line = str_new(s->buf.buf, n);
			if (!utf8_valid(line)) {
				session_close(s);
				break;
			}
			uv_timer_start(&s->timer, on_timeout, IDENTD_TIMEOUT_MS, 0);
			handle_line(s, line);
			memmove(s->buf.buf, s->buf.buf + n, s->buf.len - n);
			strbuf_truncate(&s->buf, s->buf.len - n);
		}
	} else if (nread < 0) {
		session_close(s);
	}
	xfree(buf->base);
}

void identd_serve(struct identd *id, uv_stream_t *stream, const char *remote_host,
		  bool trust_proxy) {
	struct session *s = xcalloc(1, sizeof(*s));
	s->trust_proxy = trust_proxy;
	REF_TRACK(&s->ref, "identd_session");
	s->id = id;
	s->stream = stream;
	s->remote_host = xstrdup(remote_host);
	// The two handles, released by their close callbacks, plus writes
	s->pending = 2;
	stream->data = s;
	uv_timer_init(stream->loop, &s->timer);
	s->timer.data = s;
	uv_timer_start(&s->timer, on_timeout, IDENTD_TIMEOUT_MS, 0);
	if (uv_read_start(stream, on_alloc, on_read) != 0) {
		session_close(s);
	}
}
