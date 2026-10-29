// Logging in the format of Go's standard log package, like soju-rs:
// "2006/01/02 15:04:05 <prefix><message>" on stderr.
#pragma once

#include <stdarg.h>
#include <stdatomic.h>

// The debug flag shared by every logger (soju -debug, "server debug")
struct log_root {
	atomic_bool debug;
};

// A logger with a prefix. Loggers are owned values: log_with_prefix
// returns a new one which must be freed with log_free.
struct logger {
	struct log_root *root;
	char *prefix;
};

void log_init(struct logger *l, struct log_root *root);
// A new logger appending prefix to l's
void log_with_prefix(struct logger *dst, const struct logger *l, const char *prefix);
void log_free(struct logger *l);
[[nodiscard]] bool log_debug_enabled(const struct logger *l);
// Sets the debug flag, returning the previous value
bool log_set_debug(const struct logger *l, bool debug);

[[gnu::format(printf, 2, 3)]] void log_printf(const struct logger *l, const char *fmt, ...);
[[gnu::format(printf, 2, 0)]] void log_vprintf(const struct logger *l, const char *fmt, va_list ap);

// logf!/debugf! of soju-rs (named so they don't clash with math.h's logf)
#define log_info(l, ...) log_printf((l), __VA_ARGS__)
#define log_debug(l, ...)                             \
	do {                                          \
		if (log_debug_enabled(l)) {           \
			log_printf((l), __VA_ARGS__); \
		}                                     \
	} while (0)

// Where log lines go (stderr by default); tests capture them
typedef void (*log_sink_fn)(const char *line, void *data);
void log_set_sink(log_sink_fn fn, void *data);
