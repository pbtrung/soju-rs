// Memory allocation. Allocation failure aborts the process (PLAN.md 2.1
// rule 5), so callers never check for NULL. All memory returned by these
// functions is freed with xfree(); memory from libraries (utf8proc, yyjson,
// OpenSSL, ...) is freed with the library's own function.
#pragma once

#include <stddef.h>

[[nodiscard]] void *xmalloc(size_t n);
[[nodiscard]] void *xcalloc(size_t count, size_t size);
[[nodiscard]] void *xrealloc(void *p, size_t n);
// realloc to count * size bytes, aborting on overflow
[[nodiscard]] void *xreallocarray(void *p, size_t count, size_t size);
[[nodiscard]] char *xstrdup(const char *s);
// Copies n bytes of s and NUL-terminates the copy
[[nodiscard]] char *xstrndup(const char *s, size_t n);
[[nodiscard]] void *xmemdup(const void *p, size_t n);
// asprintf that aborts on failure
[[nodiscard]] [[gnu::format(printf, 1, 2)]] char *xasprintf(const char *fmt, ...);
void xfree(void *p);

// Size arithmetic that aborts on overflow (rule 10: no unchecked size
// arithmetic fed to an allocator or memcpy)
[[nodiscard]] size_t size_add(size_t a, size_t b);
[[nodiscard]] size_t size_mul(size_t a, size_t b);

[[noreturn]] void die_oom(void);

// Frees the pointer when the variable goes out of scope:
//   AUTOFREE char *s = xstrdup("x");
void autofree_impl(void *pp);
#define AUTOFREE [[gnu::cleanup(autofree_impl)]]

// Test hooks: unit tests route allocations through cmocka's checked
// allocator in the debug build (PLAN.md 2.3 L1). Passing nullptr restores
// the default.
struct alloc_hooks {
	void *(*malloc)(size_t n);
	void *(*calloc)(size_t count, size_t size);
	void *(*realloc)(void *p, size_t n);
	void (*free)(void *p);
};
void alloc_set_hooks(const struct alloc_hooks *hooks);
