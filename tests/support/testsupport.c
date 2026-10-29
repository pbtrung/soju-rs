#include "testsupport.h"

#include <pthread.h>
#include <stdlib.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>

#include "base/alloc.h"
#include "base/ref.h"
#include "irc/message.h"

#ifdef SOJU_MODE_DEBUG
// cmocka's checked allocator keeps its list of blocks in thread-local
// storage: a block allocated by a thread which then exits (a mock server's
// connection thread) would leave the list dangling once freed elsewhere.
// Only the main test thread uses it; other threads use the system
// allocator. A tag before each block says where it came from. cmocka's
// global lists are serialized anyway (the DB thread and thread pool jobs
// allocate too).
static pthread_mutex_t alloc_mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_t main_thread;

enum : uint64_t {
	TAG_CMOCKA = 0x636d6f636b61UL,
	TAG_SYSTEM = 0x73797374656dUL,
};
static constexpr size_t TAG_SIZE = 16; // keeps the alignment

static void *tagged(void *p, uint64_t tag) {
	if (p == nullptr) {
		return nullptr;
	}
	memcpy(p, &tag, sizeof(tag));
	return (char *)p + TAG_SIZE;
}

static uint64_t tag_of(void *p, void **base) {
	*base = (char *)p - TAG_SIZE;
	uint64_t tag;
	memcpy(&tag, *base, sizeof(tag));
	return tag;
}

static void *t_malloc(size_t n) {
	if (!pthread_equal(pthread_self(), main_thread)) {
		return tagged(malloc(size_add(n, TAG_SIZE)), TAG_SYSTEM);
	}
	pthread_mutex_lock(&alloc_mu);
	void *p = _test_malloc(size_add(n, TAG_SIZE), __FILE__, __LINE__);
	pthread_mutex_unlock(&alloc_mu);
	return tagged(p, TAG_CMOCKA);
}

static void *t_calloc(size_t count, size_t size) {
	size_t n = size_mul(count, size);
	void *p = t_malloc(n);
	if (p != nullptr) {
		memset(p, 0, n);
	}
	return p;
}

static void *t_realloc(void *p, size_t n) {
	if (p == nullptr) {
		return t_malloc(n);
	}
	void *base;
	uint64_t tag = tag_of(p, &base);
	if (tag == TAG_SYSTEM) {
		return tagged(realloc(base, size_add(n, TAG_SIZE)), TAG_SYSTEM);
	}
	if (tag != TAG_CMOCKA) {
		abort();
	}
	pthread_mutex_lock(&alloc_mu);
	void *q = _test_realloc(base, size_add(n, TAG_SIZE), __FILE__, __LINE__);
	pthread_mutex_unlock(&alloc_mu);
	return tagged(q, TAG_CMOCKA);
}

static void t_free(void *p) {
	if (p == nullptr) {
		return;
	}
	void *base;
	uint64_t tag = tag_of(p, &base);
	if (tag == TAG_SYSTEM) {
		free(base);
		return;
	}
	if (tag != TAG_CMOCKA) {
		abort();
	}
	pthread_mutex_lock(&alloc_mu);
	_test_free(base, __FILE__, __LINE__);
	pthread_mutex_unlock(&alloc_mu);
}

static const struct alloc_hooks cmocka_hooks = {
	.malloc = t_malloc,
	.calloc = t_calloc,
	.realloc = t_realloc,
	.free = t_free,
};
#endif

int t_run(const char *name, const struct CMUnitTest *tests, size_t n) {
	// Like soju (src/main/soju.c): writes to closed peers fail with EPIPE
	signal(SIGPIPE, SIG_IGN);
#ifdef SOJU_MODE_DEBUG
	main_thread = pthread_self();
	alloc_set_hooks(&cmocka_hooks);
#endif
	int failed = _cmocka_run_group_tests(name, tests, n, nullptr, nullptr);
#ifdef SOJU_MODE_DEBUG
	alloc_set_hooks(nullptr);
#endif
	if (ref_dump_live(stderr) != 0) {
		fprintf(stderr, "%s: objects left alive after the tests\n", name);
		failed++;
	}
	return failed != 0 ? 1 : 0;
}

void t_assert_str_eq(struct str s, const char *cstr, const char *file, int line) {
	if (!str_eq_cstr(s, cstr)) {
		cmocka_print_error("%s:%d: \"%.*s\" != \"%s\"\n", file, line, STR_FMT(s), cstr);
		_fail(file, line);
	}
}

void t_assert_cstr_eq(const char *a, const char *b, const char *file, int line) {
	if (a == nullptr || b == nullptr ? a != b : strcmp(a, b) != 0) {
		cmocka_print_error("%s:%d: \"%s\" != \"%s\"\n", file, line,
				   a != nullptr ? a : "(null)", b != nullptr ? b : "(null)");
		_fail(file, line);
	}
}

void t_assert_msg_str(const struct irc_msg *m, const char *want, const char *file, int line) {
	char *s = irc_msg_to_string(m);
	bool eq = strcmp(s, want) == 0;
	if (!eq) {
		cmocka_print_error("%s:%d: \"%s\" != \"%s\"\n", file, line, s, want);
	}
	xfree(s);
	if (!eq) {
		_fail(file, line);
	}
}
