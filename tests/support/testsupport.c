#include "testsupport.h"

#include <pthread.h>
#include <stdio.h>
#include <string.h>

#include "base/alloc.h"
#include "base/ref.h"
#include "irc/message.h"

#ifdef SOJU_MODE_DEBUG
// cmocka's checked allocator keeps global lists: serialize it, since the
// DB thread and thread pool jobs allocate too
static pthread_mutex_t alloc_mu = PTHREAD_MUTEX_INITIALIZER;

static void *t_malloc(size_t n) {
	pthread_mutex_lock(&alloc_mu);
	void *p = _test_malloc(n, __FILE__, __LINE__);
	pthread_mutex_unlock(&alloc_mu);
	return p;
}

static void *t_calloc(size_t count, size_t size) {
	pthread_mutex_lock(&alloc_mu);
	void *p = _test_calloc(count, size, __FILE__, __LINE__);
	pthread_mutex_unlock(&alloc_mu);
	return p;
}

static void *t_realloc(void *p, size_t n) {
	pthread_mutex_lock(&alloc_mu);
	void *q = _test_realloc(p, n, __FILE__, __LINE__);
	pthread_mutex_unlock(&alloc_mu);
	return q;
}

static void t_free(void *p) {
	pthread_mutex_lock(&alloc_mu);
	_test_free(p, __FILE__, __LINE__);
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
#ifdef SOJU_MODE_DEBUG
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
