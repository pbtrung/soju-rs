#include "base/err.h"

#include "base/alloc.h"
#include "base/str.h"

bool err_set(struct err **err, int code, const char *fmt, ...) {
	if (err == nullptr) {
		return false;
	}
	struct strbuf sb = STRBUF_INIT;
	va_list ap;
	va_start(ap, fmt);
	strbuf_vprintf(&sb, fmt, ap);
	va_end(ap);
	err_free(*err);
	*err = xmalloc(sizeof(**err));
	**err = (struct err){.code = code, .msg = strbuf_take(&sb)};
	return false;
}

bool err_wrap(struct err **err, const char *fmt, ...) {
	if (err == nullptr || *err == nullptr) {
		return false;
	}
	struct strbuf sb = STRBUF_INIT;
	va_list ap;
	va_start(ap, fmt);
	strbuf_vprintf(&sb, fmt, ap);
	va_end(ap);
	strbuf_printf(&sb, ": %s", (*err)->msg);
	xfree((*err)->msg);
	(*err)->msg = strbuf_take(&sb);
	return false;
}

const char *err_msg(const struct err *e) {
	return e != nullptr ? e->msg : "(no error)";
}

void err_free(struct err *e) {
	if (e != nullptr) {
		xfree(e->msg);
		xfree(e);
	}
}
