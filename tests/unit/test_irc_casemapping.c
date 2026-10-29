// Ported from soju-rs src/xirc/casemapping.rs tests
#include "testsupport.h"

#include <stdint.h>
#include <stdio.h>

#include "base/alloc.h"
#include "irc/casemapping.h"

static void cm_eq(enum casemapping cm, const char *in, const char *want) {
	char *s = casemap_cstr(cm, in);
	assert_string_equal(s, want);
	xfree(s);
}

static void test_casemaps(void **state) {
	(void)state;
	cm_eq(CASEMAPPING_ASCII, "ABC{}~\xC3\xA9", "abc{}~\xC3\xA9");
	cm_eq(CASEMAPPING_RFC1459, "A{}\\~", "a[]|^");
	cm_eq(CASEMAPPING_RFC1459_STRICT, "A{}\\~", "a[]|~");
}

#define V(n) ((void *)(uintptr_t)(n))
#define N(p) ((uintptr_t)(p))

static void test_map_shrinks(void **state) {
	(void)state;
	struct cmmap m;
	cmmap_init(&m, CASEMAPPING_ASCII, nullptr);
	char k[32];
	for (int i = 0; i < 1000; i++) {
		snprintf(k, sizeof(k), "Nick%d", i);
		cmmap_set(&m, k, V(i + 1));
	}
	for (int i = 0; i < 990; i++) {
		snprintf(k, sizeof(k), "NICK%d", i);
		assert_int_equal(N(cmmap_take(&m, k)), i + 1);
	}
	assert_int_equal(cmmap_len(&m), 10);
	assert_true(cmmap_capacity(&m) < 100);
	assert_int_equal(N(cmmap_get(&m, "nick995")), 996);
	cmmap_free(&m);
}

static void keys_eq(const struct cmmap *m, const char *const *want, size_t n) {
	size_t i = 0;
	const char *k;
	for (size_t it = 0; cmmap_next(m, &it, &k, nullptr); i++) {
		assert_true(i < n);
		assert_string_equal(k, want[i]);
	}
	assert_int_equal(i, n);
}

static void test_map(void **state) {
	(void)state;
	struct cmmap m;
	cmmap_init(&m, CASEMAPPING_RFC1459, nullptr);
	cmmap_set(&m, "Foo{", V(1));
	assert_true(cmmap_has(&m, "foo["));
	cmmap_set(&m, "FOO[", V(2));
	assert_int_equal(N(cmmap_get(&m, "foo{")), 2);
	// The original spelling is kept
	keys_eq(&m, (const char *[]){"Foo{"}, 1);
	cmmap_set_casemapping(&m, CASEMAPPING_ASCII);
	assert_false(cmmap_has(&m, "foo["));
	assert_true(cmmap_has(&m, "FOO{"));
	keys_eq(&m, (const char *[]){"Foo{"}, 1);
	assert_int_equal(N(cmmap_take(&m, "foo{")), 2);
	assert_int_equal(cmmap_len(&m), 0);
	cmmap_free(&m);
}

static void test_map_spellings(void **state) {
	(void)state;
	struct cmmap m;
	cmmap_init(&m, CASEMAPPING_ASCII, nullptr);
	// Already case-mapped keys don't store a second spelling
	cmmap_set(&m, "lower", V(1));
	cmmap_set(&m, "Mixed", V(2));
	const char *sp;
	assert_true(cmmap_peek_spelling(&m, "lower", &sp));
	assert_null(sp);
	assert_true(cmmap_peek_spelling(&m, "mixed", &sp));
	assert_string_equal(sp, "Mixed");
	keys_eq(&m, (const char *[]){"lower", "Mixed"}, 2);
	const char *k;
	void *v;
	for (size_t it = 0; cmmap_next(&m, &it, &k, &v);) {
		*cmmap_value_slot(&m, it) = V(N(v) + strlen(k));
	}
	assert_int_equal(N(cmmap_get(&m, "lower")), 6);
	assert_int_equal(N(cmmap_get(&m, "mixed")), 7);
	cmmap_free(&m);

	// A spelling which becomes the case-mapped key isn't stored again
	cmmap_init(&m, CASEMAPPING_ASCII, nullptr);
	cmmap_set(&m, "a[", V(1));
	cmmap_set_casemapping(&m, CASEMAPPING_RFC1459);
	keys_eq(&m, (const char *[]){"a["}, 1);
	assert_true(cmmap_peek_spelling(&m, "a[", &sp));
	assert_null(sp);
	cmmap_set(&m, "B{", V(2));
	cmmap_set_casemapping(&m, CASEMAPPING_ASCII);
	keys_eq(&m, (const char *[]){"a[", "B{"}, 2);
	assert_int_equal(N(cmmap_get(&m, "b{")), 2);
	cmmap_free(&m);
}

// C-only: owned values are freed, and removal moves the last key into
// the hole like IndexMap::swap_remove
static void test_map_owned_swap(void **state) {
	(void)state;
	struct cmmap m;
	cmmap_init(&m, CASEMAPPING_ASCII, xfree);
	cmmap_set(&m, "a", xstrdup("1"));
	cmmap_set(&m, "b", xstrdup("2"));
	cmmap_set(&m, "c", xstrdup("3"));
	cmmap_set(&m, "A", xstrdup("4"));
	assert_true(cmmap_del(&m, "a"));
	assert_false(cmmap_del(&m, "a"));
	keys_eq(&m, (const char *[]){"c", "b"}, 2);
	cmmap_free(&m);
}

T_MAIN(cmocka_unit_test(test_casemaps), cmocka_unit_test(test_map_shrinks),
       cmocka_unit_test(test_map), cmocka_unit_test(test_map_spellings),
       cmocka_unit_test(test_map_owned_swap))
