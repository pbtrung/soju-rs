// Tests of src/base/ (no Rust counterpart: soju-rs uses std)
#include "testsupport.h"

#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "base/alloc.h"
#include "base/arena.h"
#include "base/err.h"
#include "base/map.h"
#include "base/ref.h"
#include "base/str.h"
#include "base/vec.h"

static void test_str_split(void **state) {
	(void)state;
	struct str a, b;
	assert_true(str_split_once(STR_LIT("k=v=w"), '=', &a, &b));
	assert_str_eq(a, "k");
	assert_str_eq(b, "v=w");
	assert_false(str_split_once(STR_LIT("kv"), '=', &a, &b));

	struct str rest = STR_LIT("a;;b"), f;
	const char *want[] = {"a", "", "b"};
	size_t n = 0;
	while (str_next_field(&rest, ';', &f)) {
		assert_true(n < 3);
		assert_str_eq(f, want[n]);
		n++;
	}
	assert_int_equal(n, 3);

	rest = STR_EMPTY;
	n = 0;
	while (str_next_field(&rest, ';', &f)) {
		assert_int_equal(f.len, 0);
		n++;
	}
	assert_int_equal(n, 1);
}

static void test_str_misc(void **state) {
	(void)state;
	assert_str_eq(str_trim_ascii(STR_LIT(" \t x y \r\n")), "x y");
	assert_true(str_starts_with(STR_LIT("abc"), STR_LIT("ab")));
	assert_false(str_starts_with(STR_LIT("a"), STR_LIT("ab")));
	assert_true(str_ends_with(STR_LIT("abc"), STR_LIT("bc")));
	assert_int_equal(str_find(STR_LIT("a :b"), STR_LIT(" :")), 1);
	assert_int_equal(str_find(STR_LIT("ab"), STR_LIT("abc")), -1);
	assert_int_equal(str_rindex(STR_LIT("a.b.c"), '.'), 3);
	assert_str_eq(str_slice(STR_LIT("hello"), 1, 99), "ello");
	assert_str_eq(str_slice(STR_LIT("hello"), 9, 2), "");
	assert_true(str_eq_ignore_ascii_case(STR_LIT("PrivMsg"), STR_LIT("PRIVMSG")));
	assert_true(str_cmp(STR_LIT("a"), STR_LIT("ab")) < 0);
}

static void test_str_parse_int(void **state) {
	(void)state;
	long long v;
	unsigned long long u;
	assert_true(str_parse_i64(STR_LIT("-9223372036854775808"), &v));
	assert_true(v == INT64_MIN);
	assert_true(str_parse_i64(STR_LIT("+42"), &v));
	assert_int_equal(v, 42);
	assert_false(str_parse_i64(STR_LIT("9223372036854775808"), &v));
	assert_false(str_parse_i64(STR_LIT(""), &v));
	assert_false(str_parse_i64(STR_LIT("-"), &v));
	assert_false(str_parse_i64(STR_LIT(" 1"), &v));
	assert_false(str_parse_i64(STR_LIT("1x"), &v));
	assert_true(str_parse_u64(STR_LIT("18446744073709551615"), &u));
	assert_false(str_parse_u64(STR_LIT("18446744073709551616"), &u));
	assert_false(str_parse_u64(STR_LIT("-1"), &u));
}

static void test_strbuf(void **state) {
	(void)state;
	struct strbuf sb = STRBUF_INIT;
	assert_string_equal(strbuf_cstr(&sb), "");
	for (int i = 0; i < 100; i++) {
		strbuf_printf(&sb, "%d,", i);
	}
	assert_true(str_starts_with(strbuf_str(&sb), STR_LIT("0,1,2,")));
	assert_true(str_ends_with(strbuf_str(&sb), STR_LIT("98,99,")));
	strbuf_truncate(&sb, 2);
	assert_string_equal(strbuf_cstr(&sb), "0,");
	char *s = strbuf_take(&sb);
	assert_string_equal(s, "0,");
	assert_null(sb.buf);
	xfree(s);
	s = strbuf_take(&sb);
	assert_string_equal(s, "");
	xfree(s);
}

static void free_int(void *p) {
	xfree(p);
}

static int *new_int(int v) {
	int *p = xmalloc(sizeof(*p));
	*p = v;
	return p;
}

static void test_map(void **state) {
	(void)state;
	struct strmap m;
	strmap_init(&m, free_int);
	char key[16];
	for (int i = 0; i < 1000; i++) {
		snprintf(key, sizeof(key), "k%d", i);
		strmap_set(&m, key, new_int(i));
	}
	assert_int_equal(strmap_len(&m), 1000);
	for (int i = 0; i < 1000; i += 2) {
		snprintf(key, sizeof(key), "k%d", i);
		assert_true(strmap_remove(&m, key));
	}
	assert_false(strmap_remove(&m, "k0"));
	strmap_set(&m, "k1", new_int(-1));
	assert_int_equal(*(int *)strmap_get(&m, "k1"), -1);
	assert_null(strmap_get(&m, "k2"));

	// Insertion order is kept, replaced keys keep their place
	size_t it = 0, n = 0;
	const char *k;
	void *v;
	long long prev = -2;
	while (strmap_next(&m, &it, &k, &v)) {
		long long i;
		assert_true(str_parse_i64(str_from(k + 1), &i));
		assert_true(i > prev);
		prev = i;
		n++;
	}
	assert_int_equal(n, 500);

	int *taken = strmap_take(&m, "k3");
	assert_int_equal(*taken, 3);
	xfree(taken);
	assert_false(strmap_has(&m, "k3"));
	strmap_free(&m);
	assert_int_equal(strmap_len(&m), 0);
	strmap_set(&m, "again", new_int(1));
	strmap_free(&m);
}

static void test_vec(void **state) {
	(void)state;
	VEC(int) v = {0};
	for (int i = 0; i < 10; i++) {
		vec_push(&v, i);
	}
	vec_remove(&v, 0);
	vec_insert(&v, 9, 100);
	vec_insert(&v, 0, -1);
	assert_int_equal(v.len, 11);
	assert_int_equal(v.data[0], -1);
	assert_int_equal(v.data[1], 1);
	assert_int_equal(vec_last(&v), 100);
	vec_free(&v);

	strvec sv = {0};
	strvec_push(&sv, "a");
	strvec_push(&sv, "b");
	assert_int_equal(strvec_index(&sv, "b"), 1);
	assert_int_equal(strvec_index(&sv, "c"), -1);
	strvec copy = {0};
	strvec_copy(&copy, &sv);
	strvec_free(&sv);
	assert_string_equal(copy.data[1], "b");
	strvec_free(&copy);
}

static void test_arena(void **state) {
	(void)state;
	struct arena a = ARENA_INIT;
	char *first = arena_strdup(&a, "first");
	for (int i = 0; i < 200; i++) {
		char *s = arena_strdup(&a, "0123456789");
		assert_string_equal(s, "0123456789");
	}
	char *big = arena_alloc(&a, 10000);
	memset(big, 'x', 10000);
	char *after = arena_strdup(&a, "after");
	assert_string_equal(first, "first");
	assert_string_equal(after, "after");
	assert_true(((uintptr_t)arena_alloc(&a, 1) % alignof(max_align_t)) == 0);
	arena_free(&a);
	assert_null(a.head);
}

static void test_err(void **state) {
	(void)state;
	struct err *e = nullptr;
	assert_false(err_set(&e, 7, "bad %s", "thing"));
	assert_false(err_wrap(&e, "context %d", 1));
	assert_string_equal(err_msg(e), "context 1: bad thing");
	assert_int_equal(e->code, 7);
	err_free(e);
	assert_false(err_set(nullptr, 0, "ignored"));
}

struct thing {
	struct ref_node ref;
};

static void test_ref(void **state) {
	(void)state;
	struct thing *t = xcalloc(1, sizeof(*t));
	REF_TRACK(&t->ref, "thing");
	assert_int_equal(ref_live_count("thing"), SOJU_REF_TRACKING ? 1 : 0);
	assert_int_equal(ref_live_count("other"), 0);
	ref_untrack(&t->ref);
	xfree(t);
	assert_int_equal(ref_live_count(nullptr), 0);

	struct refcount rc;
	refcount_init(&rc);
	refcount_inc(&rc);
	assert_false(refcount_dec(&rc));
	assert_true(refcount_dec(&rc));
}

static void test_size_arith(void **state) {
	(void)state;
	assert_int_equal(size_add(1, 2), 3);
	assert_int_equal(size_mul(3, 4), 12);
}

T_MAIN(cmocka_unit_test(test_str_split), cmocka_unit_test(test_str_misc),
       cmocka_unit_test(test_str_parse_int), cmocka_unit_test(test_strbuf),
       cmocka_unit_test(test_map), cmocka_unit_test(test_vec), cmocka_unit_test(test_arena),
       cmocka_unit_test(test_err), cmocka_unit_test(test_ref), cmocka_unit_test(test_size_arith))
