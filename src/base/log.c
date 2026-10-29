#include "base/log.h"

#include <stdio.h>
#include <time.h>

#include "base/alloc.h"
#include "base/str.h"

static log_sink_fn sink;
static void *sink_data;

void log_set_sink(log_sink_fn fn, void *data) {
	sink = fn;
	sink_data = data;
}

void log_init(struct logger *l, struct log_root *root) {
	*l = (struct logger){.root = root, .prefix = xstrdup("")};
}

void log_with_prefix(struct logger *dst, const struct logger *l, const char *prefix) {
	*dst = (struct logger){.root = l->root, .prefix = xasprintf("%s%s", l->prefix, prefix)};
}

void log_free(struct logger *l) {
	xfree(l->prefix);
	l->prefix = nullptr;
}

bool log_debug_enabled(const struct logger *l) {
	return atomic_load(&l->root->debug);
}

bool log_set_debug(const struct logger *l, bool debug) {
	return atomic_exchange(&l->root->debug, debug);
}

void log_vprintf(const struct logger *l, const char *fmt, va_list ap) {
	char now[32] = "";
	time_t t = time(nullptr);
	struct tm tm;
	if (localtime_r(&t, &tm) != nullptr) {
		strftime(now, sizeof(now), "%Y/%m/%d %H:%M:%S", &tm);
	}
	AUTO_STRBUF struct strbuf sb = STRBUF_INIT;
	strbuf_printf(&sb, "%s %s", now, l->prefix);
	strbuf_vprintf(&sb, fmt, ap);
	strbuf_append_char(&sb, '\n');
	if (sink != nullptr) {
		sink(strbuf_cstr(&sb), sink_data);
	} else {
		fwrite(sb.buf, 1, sb.len, stderr);
	}
}

void log_printf(const struct logger *l, const char *fmt, ...) {
	va_list ap;
	va_start(ap, fmt);
	log_vprintf(l, fmt, ap);
	va_end(ap);
}
