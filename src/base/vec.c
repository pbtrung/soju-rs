#include "base/vec.h"

void vec_reserve_impl(void **data, size_t *cap, size_t len, size_t n, size_t elem_size) {
	size_t need = size_add(len, n);
	if (need <= *cap) {
		return;
	}
	size_t c = *cap != 0 ? *cap : 4;
	while (c < need) {
		c = size_mul(c, 2);
	}
	*data = xreallocarray(*data, c, elem_size);
	*cap = c;
}

void strvec_push(strvec *v, const char *s) {
	vec_push(v, xstrdup(s));
}

void strvec_push_take(strvec *v, char *s) {
	vec_push(v, s);
}

static void free_str(char **s) {
	xfree(*s);
}

void strvec_free(strvec *v) {
	vec_free_with(v, free_str);
}

ptrdiff_t strvec_index(const strvec *v, const char *s) {
	for (size_t i = 0; i < v->len; i++) {
		if (strcmp(v->data[i], s) == 0) {
			return (ptrdiff_t)i;
		}
	}
	return -1;
}

void strvec_copy(strvec *dst, const strvec *src) {
	vec_reserve(dst, src->len);
	for (size_t i = 0; i < src->len; i++) {
		strvec_push(dst, src->data[i]);
	}
}
