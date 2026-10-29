// Ported from soju-rs src/util.rs tests (test cases from Go's path package)
#include "testsupport.h"

#include "base/path_match.h"

static void test_go_match_tests(void **state) {
	(void)state;
	static const struct {
		const char *pattern, *name;
		int want; // 1: match, 0: no match, -1: bad pattern
	} cases[] = {
		{"abc", "abc", 1},
		{"*", "abc", 1},
		{"*c", "abc", 1},
		{"a*", "a", 1},
		{"a*", "abc", 1},
		{"a*", "ab/c", 0},
		{"a*/b", "abc/b", 1},
		{"a*/b", "a/c/b", 0},
		{"a*b*c*d*e*/f", "axbxcxdxe/f", 1},
		{"a*b*c*d*e*/f", "axbxcxdxexxx/f", 1},
		{"a*b*c*d*e*/f", "axbxcxdxe/xxx/f", 0},
		{"a*b*c*d*e*/f", "axbxcxdxexxx/fff", 0},
		{"a*b?c*x", "abxbbxdbxebxczzx", 1},
		{"a*b?c*x", "abxbbxdbxebxczzy", 0},
		{"ab[c]", "abc", 1},
		{"ab[b-d]", "abc", 1},
		{"ab[e-g]", "abc", 0},
		{"ab[^c]", "abc", 0},
		{"ab[^b-d]", "abc", 0},
		{"ab[^e-g]", "abc", 1},
		{"a\\*b", "a*b", 1},
		{"a\\*b", "ab", 0},
		{"a?b", "a☺b", 1},
		{"a[^a]b", "a☺b", 1},
		{"a???b", "a☺b", 0},
		{"a[^a][^a][^a]b", "a☺b", 0},
		{"[a-ζ]*", "α", 1},
		{"*[a-ζ]", "A", 0},
		{"a?b", "a/b", 0},
		{"a*b", "a/b", 0},
		{"[\\]a]", "]", 1},
		{"[\\-]", "-", 1},
		{"[x\\-]", "x", 1},
		{"[x\\-]", "-", 1},
		{"[x\\-]", "z", 0},
		{"[\\-x]", "x", 1},
		{"[\\-x]", "-", 1},
		{"[\\-x]", "a", 0},
		{"[]a]", "]", -1},
		{"[-]", "-", -1},
		{"[x-]", "x", -1},
		{"[x-]", "-", -1},
		{"[x-]", "z", -1},
		{"[-x]", "x", -1},
		{"[-x]", "-", -1},
		{"[-x]", "a", -1},
		{"\\", "a", -1},
		{"[a-b-c]", "a", -1},
		{"[", "a", -1},
		{"[^", "a", -1},
		{"[^bc", "a", -1},
		{"a[", "a", -1},
		{"a[", "ab", -1},
		{"a[", "x", -1},
		{"a/b[", "x", -1},
		{"*x", "xxx", 1},
	};
	for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
		bool m = false;
		bool ok = path_match(str_from(cases[i].pattern), str_from(cases[i].name), &m);
		int got = ok ? m : -1;
		if (got != cases[i].want) {
			fail_msg("path_match(\"%s\", \"%s\") = %d, want %d", cases[i].pattern,
				 cases[i].name, got, cases[i].want);
		}
	}
}

T_MAIN(cmocka_unit_test(test_go_match_tests))
