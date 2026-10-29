// Shared cmocka setup for the unit tests (PLAN.md 2.3 L1).
//
//   static void test_x(void **state) { ... }
//   T_MAIN(cmocka_unit_test(test_x), ...)
//
// CTest runs each test in its own process (CMOCKA_TEST_FILTER). After the
// tests, no tracked object may be left alive; in the debug build every
// xmalloc goes through cmocka's checked allocator too, which fails a test
// that leaks or overruns a block.
#pragma once

#include <setjmp.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>

#include <cmocka.h>

#include "base/str.h"

int t_run(const char *name, const struct CMUnitTest *tests, size_t n);

#define T_MAIN(...)                                                                       \
	int main(void) {                                                                  \
		static const struct CMUnitTest t_tests_[] = {__VA_ARGS__};                \
		return t_run(__FILE__, t_tests_, sizeof(t_tests_) / sizeof(t_tests_[0])); \
	}

// Asserts that a struct str equals a C string, printing both otherwise
#define assert_str_eq(s, cstr) t_assert_str_eq((s), (cstr), __FILE__, __LINE__)
void t_assert_str_eq(struct str s, const char *cstr, const char *file, int line);

// Asserts two possibly-null C strings are equal
#define assert_cstr_eq(a, b) t_assert_cstr_eq((a), (b), __FILE__, __LINE__)
void t_assert_cstr_eq(const char *a, const char *b, const char *file, int line);

struct irc_msg;
// Asserts that a message formats to want
#define assert_msg_str(m, want) t_assert_msg_str((m), (want), __FILE__, __LINE__)
void t_assert_msg_str(const struct irc_msg *m, const char *want, const char *file, int line);
// Asserts that a strbuf holds want
#define assert_sb_eq(sb, want) t_assert_str_eq(strbuf_str(sb), (want), __FILE__, __LINE__)
