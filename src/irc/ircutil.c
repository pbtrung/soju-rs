#include "irc/ircutil.h"

#include <stdlib.h>

#include "base/alloc.h"
#include "base/utf8.h"
#include "irc/numerics.h"

char *redact_for_log(const struct irc_msg *msg) {
	static const char *const secret[] = {"PASS", "AUTHENTICATE", "OPER", "REGISTER", "VERIFY"};
	for (size_t i = 0; i < sizeof(secret) / sizeof(secret[0]); i++) {
		if (cstr_eq(msg->command, secret[i])) {
			return xasprintf("%s <redacted>", msg->command);
		}
	}
	return irc_msg_to_string(msg);
}

bool user_modes_has(const struct user_modes *m, uint8_t c) {
	return memchr(m->s.buf != nullptr ? m->s.buf : "", c, m->s.len) != nullptr;
}

void user_modes_add(struct user_modes *m, uint8_t c) {
	// Mode letters are ASCII: other bytes are ignored
	if (c < 0x80 && !user_modes_has(m, c)) {
		strbuf_append_char(&m->s, (char)c);
	}
}

void user_modes_del(struct user_modes *m, uint8_t c) {
	if (m->s.buf == nullptr) {
		return;
	}
	char *p = memchr(m->s.buf, c, m->s.len);
	if (p != nullptr) {
		memmove(p, p + 1, m->s.len - (size_t)(p - m->s.buf));
		m->s.len--;
	}
}

bool user_modes_apply(struct user_modes *m, struct str s, struct err **err) {
	uint8_t plus_minus = 0;
	for (size_t i = 0; i < s.len; i++) {
		uint8_t c = (uint8_t)s.p[i];
		if (c == '+' || c == '-') {
			plus_minus = c;
		} else if (plus_minus == '+') {
			user_modes_add(m, c);
		} else if (plus_minus == '-') {
			user_modes_del(m, c);
		} else {
			AUTOFREE char *q = utf8_debug_cstr(s);
			return err_set(err, 0, "malformed modestring %s: missing plus/minus", q);
		}
	}
	return true;
}

void user_modes_free(struct user_modes *m) {
	strbuf_free(&m->s);
}

void channel_mode_types_set(struct channel_mode_types *t, uint8_t mode,
			    enum channel_mode_type type) {
	for (size_t i = 0; i < t->v.len; i++) {
		if (t->v.data[i].mode == mode) {
			t->v.data[i].type = type;
			return;
		}
	}
	vec_push(&t->v, (struct channel_mode_type_entry){mode, type});
}

bool channel_mode_types_get(const struct channel_mode_types *t, uint8_t mode,
			    enum channel_mode_type *out) {
	for (size_t i = 0; i < t->v.len; i++) {
		if (t->v.data[i].mode == mode) {
			*out = t->v.data[i].type;
			return true;
		}
	}
	return false;
}

void channel_mode_types_free(struct channel_mode_types *t) {
	vec_free(&t->v);
}

void std_channel_modes(struct channel_mode_types *out) {
	static const struct channel_mode_type_entry std[] = {
		{'b', CHANMODE_A}, // ban list
		{'e', CHANMODE_A}, // ban exception list
		{'I', CHANMODE_A}, // invite exception list
		{'k', CHANMODE_B}, // channel key
		{'l', CHANMODE_C}, // channel user limit
		{'i', CHANMODE_D}, // channel is invite-only
		{'m', CHANMODE_D}, // channel is moderated
		{'n', CHANMODE_D}, // channel has no external messages
		{'s', CHANMODE_D}, // channel is secret
		{'t', CHANMODE_D}, // channel has protected topic
	};
	for (size_t i = 0; i < sizeof(std) / sizeof(std[0]); i++) {
		channel_mode_types_set(out, std[i].mode, std[i].type);
	}
}

void channel_modes_set(struct channel_modes *cm, uint8_t mode, const char *value) {
	for (size_t i = 0; i < cm->v.len; i++) {
		if (cm->v.data[i].mode == mode) {
			char *v = xstrdup(value);
			xfree(cm->v.data[i].value);
			cm->v.data[i].value = v;
			return;
		}
	}
	vec_push(&cm->v, (struct channel_mode){mode, xstrdup(value)});
}

void channel_modes_remove(struct channel_modes *cm, uint8_t mode) {
	for (size_t i = 0; i < cm->v.len; i++) {
		if (cm->v.data[i].mode == mode) {
			xfree(cm->v.data[i].value);
			vec_remove(&cm->v, i);
			return;
		}
	}
}

const char *channel_modes_get(const struct channel_modes *cm, uint8_t mode) {
	for (size_t i = 0; i < cm->v.len; i++) {
		if (cm->v.data[i].mode == mode) {
			return cm->v.data[i].value;
		}
	}
	return nullptr;
}

static void free_mode(struct channel_mode *m) {
	xfree(m->value);
}

void channel_modes_free(struct channel_modes *cm) {
	vec_free_with(&cm->v, free_mode);
}

const struct membership *std_memberships(size_t *n) {
	static const struct membership std[] = {
		{'q', '~'}, // founder
		{'a', '&'}, // protected
		{'o', '@'}, // operator
		{'h', '%'}, // halfop
		{'v', '+'}, // voice
	};
	*n = sizeof(std) / sizeof(std[0]);
	return std;
}

bool apply_channel_modes(struct channel_modes *modes, struct cmmap *members,
			 const struct membership *available, size_t n_available,
			 const struct channel_mode_types *mode_types, struct str mode_str,
			 const char *const *args, size_t n_args, struct err **err) {
	size_t next = 0;
	uint8_t plus_minus = 0;
	for (size_t i = 0; i < mode_str.len; i++) {
		uint8_t mode = (uint8_t)mode_str.p[i];
		if (mode == '+' || mode == '-') {
			plus_minus = mode;
			continue;
		}
		if (plus_minus != '+' && plus_minus != '-') {
			AUTOFREE char *q = utf8_debug_cstr(mode_str);
			return err_set(err, 0, "malformed modestring %s: missing plus/minus", q);
		}

		bool is_membership = false;
		for (size_t k = 0; k < n_available; k++) {
			if (available[k].mode != mode) {
				continue;
			}
			is_membership = true;
			if (next >= n_args) {
				AUTOFREE char *q = utf8_debug_cstr(mode_str);
				// The Rust code formats the byte as a char: only ASCII
				// letters are memberships in practice
				struct strbuf mc = STRBUF_INIT;
				utf8_push(&mc, mode);
				err_set(err, 0,
					"malformed modestring %s: missing mode argument for %c%s",
					q, plus_minus, strbuf_cstr(&mc));
				strbuf_free(&mc);
				return false;
			}
			struct membership_set *ms = cmmap_get(members, args[next]);
			if (ms != nullptr) {
				if (plus_minus == '+') {
					membership_set_add(ms, available, n_available,
							   available[k]);
				} else {
					// TODO: for upstreams without multi-prefix, query the
					// user modes again
					membership_set_remove(ms, available[k]);
				}
			}
			next++;
			break;
		}
		if (is_membership) {
			continue;
		}

		enum channel_mode_type mt;
		if (!channel_mode_types_get(mode_types, mode, &mt)) {
			continue;
		}
		const char *arg = next < n_args ? args[next] : "";
		switch (mt) {
		case CHANMODE_A:
			next++;
			break;
		case CHANMODE_B:
			if (plus_minus == '+') {
				// some sensitive arguments (such as channel keys) can be
				// omitted for privacy (this will only happen for
				// RPL_CHANNELMODEIS, never for MODE messages)
				if (modes != nullptr) {
					channel_modes_set(modes, mode, arg);
				}
			} else if (modes != nullptr) {
				channel_modes_remove(modes, mode);
			}
			next++;
			break;
		case CHANMODE_C:
			if (plus_minus == '+') {
				if (modes != nullptr) {
					channel_modes_set(modes, mode, arg);
				}
				next++;
				break;
			}
			[[fallthrough]];
		case CHANMODE_D:
			if (modes != nullptr) {
				if (plus_minus == '+') {
					channel_modes_set(modes, mode, "");
				} else {
					channel_modes_remove(modes, mode);
				}
			}
			break;
		}
	}
	return true;
}

void format_channel_modes(const struct channel_modes *cm, struct strbuf *mode_str, strvec *params) {
	AUTO_STRBUF struct strbuf with = STRBUF_INIT;
	AUTO_STRBUF struct strbuf without = STRBUF_INIT;
	for (size_t i = 0; i < cm->v.len; i++) {
		const struct channel_mode *m = &cm->v.data[i];
		if (m->value[0] != '\0') {
			utf8_push(&with, m->mode);
			strvec_push(params, m->value);
		} else {
			utf8_push(&without, m->mode);
		}
	}
	strbuf_append_char(mode_str, '+');
	strbuf_append_str(mode_str, strbuf_str(&with));
	strbuf_append_str(mode_str, strbuf_str(&without));
}

void format_member_prefix(struct strbuf *out, const struct membership_set *ms, bool multi_prefix) {
	for (size_t i = 0; i < ms->v.len; i++) {
		utf8_push(out, ms->v.data[i].prefix);
		if (!multi_prefix) {
			break;
		}
	}
}

void strip_member_prefixes(struct strbuf *out, struct str flags, const struct membership *ms,
			   size_t n) {
	size_t i = 0;
	while (i < flags.len) {
		size_t start = i;
		uint32_t cp;
		(void)utf8_next(flags, &i, &cp);
		bool strip = false;
		for (size_t k = 0; k < n; k++) {
			if (cp == ms[k].prefix) {
				strip = true;
			}
		}
		if (!strip) {
			strbuf_append(out, flags.p + start, i - start);
		}
	}
}

void copy_client_tags(struct irc_tags *out, const struct irc_tags *tags) {
	for (size_t i = 0; i < tags->v.len; i++) {
		if (tags->v.data[i].key[0] == '+') {
			irc_tags_set(out, tags->v.data[i].key, tags->v.data[i].value);
		}
	}
}

// inspired from weechat.look.highlight_regex
static bool is_word_boundary(bool has, uint32_t r) {
	if (!has) {
		return true;
	}
	if (r == '-' || r == '_' || r == '|') {
		return false;
	}
	return !utf8_is_alphabetic(r) && !utf8_is_numeric(r);
}

static bool is_alnum_ascii(uint32_t r) {
	return (r >= '0' && r <= '9') || (r >= 'a' && r <= 'z') || (r >= 'A' && r <= 'Z');
}

static bool is_uri_prefix(struct str text) {
	size_t i = text.len;
	uint32_t cp;
	while (utf8_prev(text, &i, &cp)) {
		if (utf8_is_whitespace(cp)) {
			text = str_slice(text, i, text.len);
			break;
		}
	}
	ptrdiff_t k = str_find(text, STR_LIT("://"));
	if (k < 0) {
		return false;
	}
	// See RFC 3986 section 3
	size_t j = (size_t)k;
	if (!utf8_prev(text, &j, &cp)) {
		return false;
	}
	return cp == '+' || cp == '-' || cp == '.' || is_alnum_ascii(cp);
}

bool is_highlight(struct str text, struct str nick) {
	if (nick.len == 0) {
		// assert! in soju-rs
		abort();
	}
	while (true) {
		ptrdiff_t k = str_find(text, nick);
		if (k < 0) {
			return false;
		}
		size_t i = (size_t)k;
		uint32_t left = 0, right = 0;
		size_t li = i, ri = i + nick.len;
		bool has_left = utf8_prev(text, &li, &left);
		bool has_right = utf8_next(text, &ri, &right);
		if (is_word_boundary(has_left, left) && is_word_boundary(has_right, right) &&
		    !is_uri_prefix(str_slice(text, 0, i))) {
			return true;
		}
		text = str_slice(text, i + nick.len, text.len);
	}
}

bool parse_chathistory_bound(struct str param, struct timestamp *out) {
	struct str kind, value;
	if (!str_split_once(param, '=', &kind, &value)) {
		return false;
	}
	if (!str_eq_cstr(kind, "timestamp")) {
		return false;
	}
	return parse_server_time(value, out);
}

bool is_numeric(const char *cmd) {
	return strlen(cmd) == 3 && cmd[0] >= '0' && cmd[0] <= '9' && cmd[1] >= '0' &&
	       cmd[1] <= '9' && cmd[2] >= '0' && cmd[2] <= '9';
}

struct irc_msg *generate_away_reply(bool away) {
	if (away) {
		return IRC_MSG(RPL_NOWAWAY, "*", "You have been marked as being away");
	}
	return IRC_MSG(RPL_UNAWAY, "*", "You are no longer marked as being away");
}

bool split_words(struct str s, strvec *words, struct err **err) {
	strvec out = {0};
	AUTO_STRBUF struct strbuf last = STRBUF_INIT;
	bool escape = false;
	uint32_t prev = ' ', delim = ' ';
	size_t i = 0;
	uint32_t r;
	while (utf8_next(s, &i, &r)) {
		if (escape) {
			// last char was a backslash, write the byte as-is.
			utf8_push(&last, r);
			escape = false;
		} else if (r == '\\') {
			escape = true;
		} else if (delim == ' ' && utf8_is_whitespace(r)) {
			// end of last word
			if (!utf8_is_whitespace(prev)) {
				strvec_push(&out, strbuf_cstr(&last));
				strbuf_reset(&last);
			}
		} else if (r == delim) {
			// delim is either " or ', switch back to space-delimited words
			delim = ' ';
		} else if (r == '"' || r == '\'') {
			if (delim == ' ') {
				// start of (double-)quoted word
				delim = r;
			} else {
				// either delim is " and r is ' or vice-versa
				utf8_push(&last, r);
			}
		} else {
			utf8_push(&last, r);
		}
		prev = r;
	}
	if (!utf8_is_whitespace(prev)) {
		strvec_push(&out, strbuf_cstr(&last));
	}
	if (delim != ' ') {
		strvec_free(&out);
		return err_set(err, 0, "unterminated quoted string");
	}
	if (escape) {
		strvec_free(&out);
		return err_set(err, 0, "unterminated backslash sequence");
	}
	for (size_t k = 0; k < out.len; k++) {
		strvec_push_take(words, out.data[k]);
	}
	vec_free(&out);
	return true;
}
