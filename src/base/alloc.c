#include "base/alloc.h"

#include <stdarg.h>
#include <stdckdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static const struct alloc_hooks default_hooks = {
	.malloc = malloc,
	.calloc = calloc,
	.realloc = realloc,
	.free = free,
};

static const struct alloc_hooks *hooks = &default_hooks;

void alloc_set_hooks(const struct alloc_hooks *h) {
	hooks = h != nullptr ? h : &default_hooks;
}

void die_oom(void) {
	static const char msg[] = "soju: out of memory\n";
	fwrite(msg, 1, sizeof(msg) - 1, stderr);
	abort();
}

size_t size_add(size_t a, size_t b) {
	size_t r;
	if (ckd_add(&r, a, b)) {
		die_oom();
	}
	return r;
}

size_t size_mul(size_t a, size_t b) {
	size_t r;
	if (ckd_mul(&r, a, b)) {
		die_oom();
	}
	return r;
}

void *xmalloc(size_t n) {
	// malloc(0) may return nullptr: always allocate at least a byte
	void *p = hooks->malloc(n != 0 ? n : 1);
	if (p == nullptr) {
		die_oom();
	}
	return p;
}

void *xcalloc(size_t count, size_t size) {
	(void)size_mul(count, size);
	void *p = hooks->calloc(count != 0 ? count : 1, size != 0 ? size : 1);
	if (p == nullptr) {
		die_oom();
	}
	return p;
}

void *xrealloc(void *p, size_t n) {
	void *q = hooks->realloc(p, n != 0 ? n : 1);
	if (q == nullptr) {
		die_oom();
	}
	return q;
}

void *xreallocarray(void *p, size_t count, size_t size) {
	return xrealloc(p, size_mul(count, size));
}

void *xmemdup(const void *p, size_t n) {
	void *q = xmalloc(n);
	if (n != 0) {
		memcpy(q, p, n);
	}
	return q;
}

char *xstrndup(const char *s, size_t n) {
	char *q = xmalloc(size_add(n, 1));
	if (n != 0) {
		memcpy(q, s, n);
	}
	q[n] = '\0';
	return q;
}

char *xstrdup(const char *s) {
	return xstrndup(s, strlen(s));
}

char *xasprintf(const char *fmt, ...) {
	va_list ap;
	va_start(ap, fmt);
	int n = vsnprintf(nullptr, 0, fmt, ap);
	va_end(ap);
	if (n < 0) {
		die_oom();
	}
	char *s = xmalloc(size_add((size_t)n, 1));
	va_start(ap, fmt);
	int m = vsnprintf(s, (size_t)n + 1, fmt, ap);
	va_end(ap);
	if (m != n) {
		die_oom();
	}
	return s;
}

void xfree(void *p) {
	if (p != nullptr) {
		hooks->free(p);
	}
}

void autofree_impl(void *pp) {
	xfree(*(void **)pp);
}
