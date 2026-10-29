#include "irc/message.h"

#include <stdint.h>

#include "base/alloc.h"
#include "base/map.h"
#include "base/utf8.h"

void irc_escape_tag_value(struct strbuf *out, struct str v) {
	for (size_t i = 0; i < v.len; i++) {
		char c = v.p[i];
		switch (c) {
		case ';':
			strbuf_append_cstr(out, "\\:");
			break;
		case ' ':
			strbuf_append_cstr(out, "\\s");
			break;
		case '\\':
			strbuf_append_cstr(out, "\\\\");
			break;
		case '\r':
			strbuf_append_cstr(out, "\\r");
			break;
		case '\n':
			strbuf_append_cstr(out, "\\n");
			break;
		default:
			strbuf_append_char(out, c);
		}
	}
}

void irc_parse_tag_value(struct strbuf *out, struct str v) {
	strbuf_reserve(out, v.len);
	for (size_t i = 0; i < v.len; i++) {
		char c = v.p[i];
		if (c != '\\') {
			strbuf_append_char(out, c);
			continue;
		}
		// A trailing backslash is dropped
		if (++i == v.len) {
			break;
		}
		char c2 = v.p[i];
		switch (c2) {
		case ':':
			c2 = ';';
			break;
		case 's':
			c2 = ' ';
			break;
		case 'r':
			c2 = '\r';
			break;
		case 'n':
			c2 = '\n';
			break;
		default:
			// '\\' and anything else: the character itself
			break;
		}
		strbuf_append_char(out, c2);
	}
}

static ptrdiff_t tags_index(const struct irc_tags *tags, struct str key) {
	for (size_t i = 0; i < tags->v.len; i++) {
		if (str_eq_cstr(key, tags->v.data[i].key)) {
			return (ptrdiff_t)i;
		}
	}
	return -1;
}

void irc_tags_set_str(struct irc_tags *tags, struct str key, struct str value) {
	ptrdiff_t i = tags_index(tags, key);
	if (i >= 0) {
		char *v = str_dup(value);
		xfree(tags->v.data[i].value);
		tags->v.data[i].value = v;
		return;
	}
	vec_push(&tags->v, (struct irc_tag){.key = str_dup(key), .value = str_dup(value)});
}

void irc_tags_set(struct irc_tags *tags, const char *key, const char *value) {
	irc_tags_set_str(tags, str_from(key), str_from(value));
}

// Sets a parsed tag, looking up its key in index (key -> position + 1)
static void parse_set(struct irc_tags *tags, struct strmap *index, struct str key,
		      struct str value) {
	AUTOFREE char *k = str_dup(key);
	uintptr_t pos = (uintptr_t)strmap_get(index, k);
	if (pos != 0) {
		struct irc_tag *t = &tags->v.data[pos - 1];
		xfree(t->value);
		t->value = str_dup(value);
		return;
	}
	vec_push(&tags->v, (struct irc_tag){.key = xstrdup(k), .value = str_dup(value)});
	strmap_set(index, k, (void *)(uintptr_t)tags->v.len);
}

void irc_tags_parse(struct irc_tags *tags, struct str s) {
	AUTO_STRBUF struct strbuf value = STRBUF_INIT;
	// Looking up previous keys linearly would make parsing quadratic in
	// the number of tags, which a client controls: past 16 tags, keys are
	// indexed (soju-rs does the same)
	size_t semicolons = 0;
	for (size_t i = 0; i < s.len; i++) {
		semicolons += s.p[i] == ';';
	}
	bool indexed = semicolons >= 16;
	struct strmap index;
	strmap_init(&index, nullptr);
	if (indexed) {
		for (size_t i = 0; i < tags->v.len; i++) {
			strmap_set(&index, tags->v.data[i].key, (void *)(uintptr_t)(i + 1));
		}
	}
	struct str rest = s, tag;
	while (str_next_field(&rest, ';', &tag)) {
		struct str k = tag, v = STR_EMPTY;
		if (str_split_once(tag, '=', &k, &v)) {
			strbuf_reset(&value);
			irc_parse_tag_value(&value, v);
			v = strbuf_str(&value);
		}
		if (indexed) {
			parse_set(tags, &index, k, v);
		} else {
			irc_tags_set_str(tags, k, v);
		}
	}
	strmap_free(&index);
}

const char *irc_tags_get(const struct irc_tags *tags, const char *key) {
	ptrdiff_t i = tags_index(tags, str_from(key));
	return i >= 0 ? tags->v.data[i].value : nullptr;
}

const char *irc_tags_get_or_empty(const struct irc_tags *tags, const char *key) {
	const char *v = irc_tags_get(tags, key);
	return v != nullptr ? v : "";
}

bool irc_tags_contains(const struct irc_tags *tags, const char *key) {
	return tags_index(tags, str_from(key)) >= 0;
}

static void free_tag(struct irc_tag *t) {
	xfree(t->key);
	xfree(t->value);
}

bool irc_tags_remove(struct irc_tags *tags, const char *key) {
	ptrdiff_t i = tags_index(tags, str_from(key));
	if (i < 0) {
		return false;
	}
	free_tag(&tags->v.data[i]);
	vec_remove(&tags->v, (size_t)i);
	return true;
}

void irc_tags_retain(struct irc_tags *tags,
		     bool (*keep)(const char *key, const char *value, void *data), void *data) {
	size_t j = 0;
	for (size_t i = 0; i < tags->v.len; i++) {
		struct irc_tag *t = &tags->v.data[i];
		if (keep(t->key, t->value, data)) {
			tags->v.data[j++] = *t;
		} else {
			free_tag(t);
		}
	}
	tags->v.len = j;
}

void irc_tags_clear(struct irc_tags *tags) {
	for (size_t i = 0; i < tags->v.len; i++) {
		free_tag(&tags->v.data[i]);
	}
	vec_clear(&tags->v);
}

void irc_tags_copy(struct irc_tags *dst, const struct irc_tags *src) {
	for (size_t i = 0; i < src->v.len; i++) {
		irc_tags_set(dst, src->v.data[i].key, src->v.data[i].value);
	}
}

void irc_tags_free(struct irc_tags *tags) {
	vec_free_with(&tags->v, free_tag);
}

void irc_tags_format(const struct irc_tags *tags, struct strbuf *out) {
	for (size_t i = 0; i < tags->v.len; i++) {
		const struct irc_tag *t = &tags->v.data[i];
		if (i > 0) {
			strbuf_append_char(out, ';');
		}
		strbuf_append_cstr(out, t->key);
		if (t->value[0] != '\0') {
			strbuf_append_char(out, '=');
			irc_escape_tag_value(out, str_from(t->value));
		}
	}
}

void irc_prefix_split(struct str s, struct str *name, struct str *user, struct str *host) {
	struct str n = s, h = STR_EMPTY, nn, u = STR_EMPTY;
	struct str before, after;
	if (str_split_once(s, '@', &before, &after)) {
		n = before;
		h = after;
	}
	if (str_split_once(n, '!', &nn, &after)) {
		n = nn;
		u = after;
	}
	*name = n;
	*user = u;
	*host = h;
}

size_t irc_prefix_formatted_len(const struct irc_prefix *p) {
	size_t n = strlen(p->name);
	if (p->user[0] != '\0') {
		n += 1 + strlen(p->user);
	}
	if (p->host[0] != '\0') {
		n += 1 + strlen(p->host);
	}
	return n;
}

void irc_prefix_format(const struct irc_prefix *p, struct strbuf *out) {
	strbuf_append_cstr(out, p->name);
	if (p->user[0] != '\0') {
		strbuf_append_char(out, '!');
		strbuf_append_cstr(out, p->user);
	}
	if (p->host[0] != '\0') {
		strbuf_append_char(out, '@');
		strbuf_append_cstr(out, p->host);
	}
}

const char *irc_parse_error_str(enum irc_parse_error e) {
	switch (e) {
	case IRC_PARSE_OK:
		return "no error";
	case IRC_PARSE_ZERO_LENGTH:
		return "irc: cannot parse zero-length message";
	case IRC_PARSE_MISSING_DATA_AFTER_PREFIX:
		return "irc: no message data after prefix";
	case IRC_PARSE_MISSING_DATA_AFTER_TAGS:
		return "irc: no message data after tags";
	case IRC_PARSE_MISSING_COMMAND:
		return "irc: missing message command";
	}
	return "irc: unknown error";
}

static struct irc_msg *msg_alloc(void) {
	struct irc_msg *msg = xcalloc(1, sizeof(*msg));
	msg->arena = ARENA_INIT;
	msg->command = arena_strdup(&msg->arena, "");
	return msg;
}

void irc_msg_free(struct irc_msg *msg) {
	if (msg == nullptr) {
		return;
	}
	irc_tags_free(&msg->tags);
	vec_free(&msg->params);
	arena_free(&msg->arena);
	xfree(msg);
}

static void set_prefix_str(struct irc_msg *msg, struct str name, struct str user, struct str host) {
	struct irc_prefix *p = arena_alloc(&msg->arena, sizeof(*p));
	p->name = arena_str(&msg->arena, name);
	p->user = arena_str(&msg->arena, user);
	p->host = arena_str(&msg->arena, host);
	msg->prefix = p;
}

static bool parse_fail(struct irc_msg *msg, enum irc_parse_error e, enum irc_parse_error *err) {
	irc_msg_free(msg);
	if (err != nullptr) {
		*err = e;
	}
	return false;
}

struct irc_msg *irc_msg_parse(struct str line, enum irc_parse_error *err) {
	while (line.len > 0 && (line.p[line.len - 1] == '\r' || line.p[line.len - 1] == '\n')) {
		line.len--;
	}
	if (err != nullptr) {
		*err = IRC_PARSE_OK;
	}
	struct irc_msg *msg = msg_alloc();
	if (line.len == 0) {
		parse_fail(msg, IRC_PARSE_ZERO_LENGTH, err);
		return nullptr;
	}

	struct str before, after;
	if (line.p[0] == '@') {
		if (!str_split_once(str_slice(line, 1, line.len), ' ', &before, &after)) {
			parse_fail(msg, IRC_PARSE_MISSING_DATA_AFTER_TAGS, err);
			return nullptr;
		}
		irc_tags_parse(&msg->tags, before);
		line = after;
		if (line.len == 0) {
			parse_fail(msg, IRC_PARSE_MISSING_COMMAND, err);
			return nullptr;
		}
	}

	if (line.p[0] == ':') {
		if (!str_split_once(str_slice(line, 1, line.len), ' ', &before, &after)) {
			parse_fail(msg, IRC_PARSE_MISSING_DATA_AFTER_PREFIX, err);
			return nullptr;
		}
		struct str name, user, host;
		irc_prefix_split(before, &name, &user, &host);
		set_prefix_str(msg, name, user, host);
		line = after;
	}

	struct str middle = line, trailing;
	bool has_trailing = false;
	if (str_split_once_str(line, STR_LIT(" :"), &before, &after)) {
		middle = before;
		trailing = after;
		has_trailing = true;
	}

	bool have_command = false;
	struct str rest = middle, field;
	while (str_next_field(&rest, ' ', &field)) {
		if (field.len == 0) {
			continue;
		}
		if (!have_command) {
			AUTO_STRBUF struct strbuf up = STRBUF_INIT;
			utf8_to_upper(&up, field);
			msg->command = arena_str(&msg->arena, strbuf_str(&up));
			have_command = true;
		} else {
			vec_push(&msg->params, arena_str(&msg->arena, field));
		}
	}
	if (!have_command) {
		parse_fail(msg, IRC_PARSE_MISSING_COMMAND, err);
		return nullptr;
	}
	if (has_trailing) {
		vec_push(&msg->params, arena_str(&msg->arena, trailing));
	}
	return msg;
}

struct irc_msg *irc_msg_new_v(const char *command, const char *const *params) {
	struct irc_msg *msg = msg_alloc();
	msg->command = arena_strdup(&msg->arena, command);
	for (size_t i = 0; params[i] != nullptr; i++) {
		vec_push(&msg->params, arena_strdup(&msg->arena, params[i]));
	}
	return msg;
}

struct irc_msg *irc_msg_clone(const struct irc_msg *src) {
	struct irc_msg *msg = msg_alloc();
	irc_tags_copy(&msg->tags, &src->tags);
	if (src->prefix != nullptr) {
		irc_msg_set_prefix_from(msg, src->prefix);
	}
	msg->command = arena_strdup(&msg->arena, src->command);
	vec_reserve(&msg->params, src->params.len);
	for (size_t i = 0; i < src->params.len; i++) {
		vec_push(&msg->params, arena_strdup(&msg->arena, src->params.data[i]));
	}
	return msg;
}

void irc_msg_set_prefix(struct irc_msg *msg, const char *name, const char *user, const char *host) {
	set_prefix_str(msg, str_from(name), str_from(user != nullptr ? user : ""),
		       str_from(host != nullptr ? host : ""));
}

void irc_msg_set_prefix_from(struct irc_msg *msg, const struct irc_prefix *prefix) {
	irc_msg_set_prefix(msg, prefix->name, prefix->user, prefix->host);
}

void irc_msg_clear_prefix(struct irc_msg *msg) {
	msg->prefix = nullptr;
}

void irc_msg_set_command(struct irc_msg *msg, const char *command) {
	msg->command = arena_strdup(&msg->arena, command);
}

void irc_msg_add_param(struct irc_msg *msg, const char *param) {
	vec_push(&msg->params, arena_strdup(&msg->arena, param));
}

void irc_msg_add_param_str(struct irc_msg *msg, struct str param) {
	vec_push(&msg->params, arena_str(&msg->arena, param));
}

void irc_msg_set_param(struct irc_msg *msg, size_t i, const char *param) {
	msg->params.data[i] = arena_strdup(&msg->arena, param);
}

void irc_msg_insert_param(struct irc_msg *msg, size_t i, const char *param) {
	vec_insert(&msg->params, i, arena_strdup(&msg->arena, param));
}

void irc_msg_remove_param(struct irc_msg *msg, size_t i) {
	vec_remove(&msg->params, i);
}

void irc_msg_truncate_params(struct irc_msg *msg, size_t n) {
	if (n < msg->params.len) {
		msg->params.len = n;
	}
}

const char *irc_msg_prefix_name(const struct irc_msg *msg) {
	return msg->prefix != nullptr ? msg->prefix->name : "";
}

const char *irc_msg_param(const struct irc_msg *msg, size_t i) {
	return i < msg->params.len ? msg->params.data[i] : "";
}

const char *irc_msg_trailing(const struct irc_msg *msg) {
	return msg->params.len > 0 ? vec_last(&msg->params) : "";
}

void irc_msg_format(const struct irc_msg *msg, struct strbuf *out) {
	if (msg->tags.v.len > 0) {
		strbuf_append_char(out, '@');
		irc_tags_format(&msg->tags, out);
		strbuf_append_char(out, ' ');
	}
	if (msg->prefix != nullptr && msg->prefix->name[0] != '\0') {
		strbuf_append_char(out, ':');
		irc_prefix_format(msg->prefix, out);
		strbuf_append_char(out, ' ');
	}
	strbuf_append_cstr(out, msg->command);
	if (msg->params.len == 0) {
		return;
	}
	for (size_t i = 0; i + 1 < msg->params.len; i++) {
		strbuf_append_char(out, ' ');
		strbuf_append_cstr(out, msg->params.data[i]);
	}
	const char *trailing = vec_last(&msg->params);
	if (trailing[0] == '\0' || strchr(trailing, ' ') != nullptr || trailing[0] == ':') {
		strbuf_append_cstr(out, " :");
	} else {
		strbuf_append_char(out, ' ');
	}
	strbuf_append_cstr(out, trailing);
}

char *irc_msg_to_string(const struct irc_msg *msg) {
	struct strbuf sb = STRBUF_INIT;
	irc_msg_format(msg, &sb);
	return strbuf_take(&sb);
}

static void msg_free_ptr(struct irc_msg **m) {
	irc_msg_free(*m);
}

void irc_msg_vec_free(irc_msg_vec *v) {
	vec_free_with(v, msg_free_ptr);
}
