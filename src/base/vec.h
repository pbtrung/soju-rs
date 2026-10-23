// Type-checked growable arrays.
//
//   VEC(int) v = {0};
//   vec_push(&v, 42);
//   for (size_t i = 0; i < v.len; i++) use(v.data[i]);
//   vec_free(&v);
//
// The vector owns its array, not what the elements point to: free those
// first (vec_free_with).
#pragma once

#include <stddef.h>
#include <string.h>

#include "base/alloc.h"

#define VEC(T)              \
	struct {            \
		T *data;    \
		size_t len; \
		size_t cap; \
	}

// Makes room for n more elements
void vec_reserve_impl(void **data, size_t *cap, size_t len, size_t n, size_t elem_size);

#define vec_reserve(v, n) \
	vec_reserve_impl((void **)&(v)->data, &(v)->cap, (v)->len, (n), sizeof(*(v)->data))

#define vec_push(v, ...)                               \
	do {                                           \
		vec_reserve((v), 1);                   \
		(v)->data[(v)->len++] = (__VA_ARGS__); \
	} while (0)

// Inserts at index i (i <= len), shifting the following elements
#define vec_insert(v, i, ...)                                       \
	do {                                                        \
		size_t vec_i_ = (i);                                \
		vec_reserve((v), 1);                                \
		memmove(&(v)->data[vec_i_ + 1], &(v)->data[vec_i_], \
			((v)->len - vec_i_) * sizeof(*(v)->data));  \
		(v)->data[vec_i_] = (__VA_ARGS__);                  \
		(v)->len++;                                         \
	} while (0)

// Removes the element at index i (i < len), shifting the following ones
#define vec_remove(v, i)                                               \
	do {                                                           \
		size_t vec_i_ = (i);                                   \
		memmove(&(v)->data[vec_i_], &(v)->data[vec_i_ + 1],    \
			((v)->len - vec_i_ - 1) * sizeof(*(v)->data)); \
		(v)->len--;                                            \
	} while (0)

#define vec_last(v) ((v)->data[(v)->len - 1])

#define vec_clear(v) ((v)->len = 0)

#define vec_free(v)                      \
	do {                             \
		xfree((v)->data);        \
		(v)->data = nullptr;     \
		(v)->len = (v)->cap = 0; \
	} while (0)

// Calls fn(&element) on every element, then frees the array
#define vec_free_with(v, fn)                                           \
	do {                                                           \
		for (size_t vec_j_ = 0; vec_j_ < (v)->len; vec_j_++) { \
			fn(&(v)->data[vec_j_]);                        \
		}                                                      \
		vec_free(v);                                           \
	} while (0)

// A vector of owned NUL-terminated strings
typedef VEC(char *) strvec;

void strvec_push(strvec *v, const char *s);
void strvec_push_take(strvec *v, char *s);
void strvec_free(strvec *v);
// Index of the first element equal to s, or -1
[[nodiscard]] ptrdiff_t strvec_index(const strvec *v, const char *s);
void strvec_copy(strvec *dst, const strvec *src);
