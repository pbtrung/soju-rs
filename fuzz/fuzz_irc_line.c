// IRC line parsing and formatting (PLAN.md 2.3 L5): no crash, and
// formatting then re-parsing a parsed message gives the same message.
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "base/alloc.h"
#include "base/utf8.h"
#include "irc/message.h"

static bool same(const struct irc_msg *a, const struct irc_msg *b) {
	if (strcmp(a->command, b->command) != 0 || a->params.len != b->params.len ||
	    a->tags.v.len != b->tags.v.len) {
		return false;
	}
	for (size_t i = 0; i < a->params.len; i++) {
		if (strcmp(a->params.data[i], b->params.data[i]) != 0) {
			return false;
		}
	}
	for (size_t i = 0; i < a->tags.v.len; i++) {
		if (strcmp(a->tags.v.data[i].key, b->tags.v.data[i].key) != 0 ||
		    strcmp(a->tags.v.data[i].value, b->tags.v.data[i].value) != 0) {
			return false;
		}
	}
	return true;
}

// The libFuzzer entry point, called by libFuzzer or fuzz/replay.c
// NOLINTNEXTLINE(misc-use-internal-linkage)
int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size);

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size) {
	// Received lines are made valid UTF-8 before parsing (like soju-rs)
	AUTO_STRBUF struct strbuf valid = STRBUF_INIT;
	utf8_lossy(&valid, str_new((const char *)data, size));
	struct str line = strbuf_str(&valid);
	// C strings stop at NUL; the connection layer never passes NUL bytes
	// on (PLAN.md: received lines are cut at the first NUL)
	if (memchr(data, '\0', size) != nullptr) {
		return 0;
	}
	struct irc_msg *m = irc_msg_parse(line, nullptr);
	if (m == nullptr) {
		return 0;
	}
	char *s = irc_msg_to_string(m);
	struct irc_msg *m2 = irc_msg_parse(str_from(s), nullptr);
	// Formatting can lose information the parser accepted: CR/LF in
	// parameters, a trailing parameter with spaces that wasn't last, an
	// empty middle parameter, or a prefix with an empty name. Only check
	// the round trip when the formatted line is canonical.
	bool canonical =
		strpbrk(s, "\r\n") == nullptr && m->command[0] != '@' && m->command[0] != ':';
	for (size_t i = 0; canonical && i + 1 < m->params.len; i++) {
		const char *p = m->params.data[i];
		canonical = p[0] != '\0' && p[0] != ':' && strchr(p, ' ') == nullptr;
	}
	if (canonical && m->prefix != nullptr && m->prefix->name[0] == '\0') {
		canonical = false;
	}
	for (size_t i = 0; canonical && i < m->tags.v.len; i++) {
		const char *k = m->tags.v.data[i].key;
		canonical = k[0] != '\0' && strpbrk(k, "=; ") == nullptr;
	}
	if (canonical && (m2 == nullptr || !same(m, m2))) {
		abort();
	}
	irc_msg_free(m2);
	xfree(s);
	struct irc_msg *c = irc_msg_clone(m);
	irc_msg_free(m);
	irc_msg_free(c);
	return 0;
}
