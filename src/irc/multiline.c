#include "irc/multiline.h"

#include "base/alloc.h"
#include "base/utf8.h"
#include "irc/xirc.h"

// The smallest line length used when splitting text
static constexpr size_t MIN_LINE_LENGTH = 64;

struct multiline_limits multiline_limits_default(void) {
	return (struct multiline_limits){
		.max_bytes = MAX_MULTILINE_BYTES,
		.max_lines = MAX_MULTILINE_LINES,
	};
}

struct multiline_limits multiline_limits_parse(struct str v) {
	struct multiline_limits limits = multiline_limits_default();
	struct str rest = v, kv;
	while (str_next_field(&rest, ',', &kv)) {
		struct str k, val;
		if (!str_split_once(kv, '=', &k, &val)) {
			continue;
		}
		unsigned long long n;
		if (!str_parse_u64(val, &n)) {
			continue;
		}
		if (str_eq_cstr(k, "max-bytes")) {
			limits.max_bytes =
				n < MAX_MULTILINE_BYTES ? (size_t)n : MAX_MULTILINE_BYTES;
		} else if (str_eq_cstr(k, "max-lines")) {
			limits.max_lines =
				n < MAX_MULTILINE_LINES ? (size_t)n : MAX_MULTILINE_LINES;
		}
	}
	return limits;
}

char *multiline_limits_cap_value(struct multiline_limits l) {
	return xasprintf("max-bytes=%zu,max-lines=%zu", l.max_bytes, l.max_lines);
}

size_t max_text_length(size_t prefix_len, const char *command, const char *target) {
	// ":<prefix> <command> <target> :<text>\r\n"
	size_t overhead = size_add(size_add(1 + 1 + 1 + 2 + 2, prefix_len),
				   size_add(strlen(command), strlen(target)));
	size_t n = MAX_MESSAGE_LENGTH > overhead ? MAX_MESSAGE_LENGTH - overhead : 0;
	return n > MIN_LINE_LENGTH ? n : MIN_LINE_LENGTH;
}

static bool is_text_command(const char *cmd) {
	return cstr_eq(cmd, "PRIVMSG") || cstr_eq(cmd, "NOTICE");
}

enum text_message_kind prepare_text_message(const struct irc_msg *msg, bool multiline,
					    size_t (*prefix_len)(void *data),
					    char *(*batch_ref)(void *data), void *data,
					    irc_msg_vec *out) {
	if (!is_text_command(msg->command) || msg->params.len < 2) {
		return TEXT_MESSAGE_SINGLE;
	}
	bool breaks = strchr(msg->params.data[1], '\n') != nullptr;
	if (!breaks && !multiline) {
		return TEXT_MESSAGE_SINGLE;
	}
	size_t max = max_text_length(prefix_len(data), msg->command, msg->params.data[0]);
	if (multiline && (breaks || strlen(msg->params.data[1]) > max)) {
		AUTOFREE char *ref = batch_ref(data);
		generate_multiline_batch(out, msg, ref, max);
		return TEXT_MESSAGE_BATCH;
	}
	if (breaks) {
		generate_multiline_fallback(out, msg, max);
		return TEXT_MESSAGE_LINES;
	}
	return TEXT_MESSAGE_SINGLE;
}

void split_multiline(multiline_line_vec *out, struct str text, size_t max) {
	if (max < MIN_LINE_LENGTH) {
		max = MIN_LINE_LENGTH;
	}
	struct str lines = text, line;
	while (str_next_field(&lines, '\n', &line)) {
		struct str rest = line;
		bool concat = false;
		while (true) {
			if (rest.len <= max) {
				// Concatenated lines can't be blank
				if (!concat || rest.len > 0) {
					vec_push(out, (struct multiline_line){rest, concat});
				}
				break;
			}
			size_t i = max;
			while (!utf8_is_char_boundary(rest, i)) {
				i--;
			}
			vec_push(out, (struct multiline_line){str_slice(rest, 0, i), concat});
			rest = str_slice(rest, i, rest.len);
			concat = true;
		}
	}
}

void generate_multiline_batch(irc_msg_vec *out, const struct irc_msg *msg, const char *batch_ref,
			      size_t max) {
	const char *target = msg->params.data[0];
	AUTOFREE char *plus = xasprintf("+%s", batch_ref);
	struct irc_msg *open = IRC_MSG("BATCH", plus, MULTILINE_BATCH_TYPE, target);
	irc_tags_copy(&open->tags, &msg->tags);
	if (msg->prefix != nullptr) {
		irc_msg_set_prefix_from(open, msg->prefix);
	}
	vec_push(out, open);

	multiline_line_vec lines = {0};
	split_multiline(&lines, str_from(msg->params.data[1]), max);
	for (size_t i = 0; i < lines.len; i++) {
		struct irc_msg *m = IRC_MSG(msg->command, target);
		irc_msg_add_param_str(m, lines.data[i].text);
		irc_tags_set(&m->tags, "batch", batch_ref);
		if (lines.data[i].concat) {
			irc_tags_set(&m->tags, MULTILINE_CONCAT_TAG, "");
		}
		if (msg->prefix != nullptr) {
			irc_msg_set_prefix_from(m, msg->prefix);
		}
		vec_push(out, m);
	}
	vec_free(&lines);
	AUTOFREE char *minus = xasprintf("-%s", batch_ref);
	vec_push(out, IRC_MSG("BATCH", minus));
}

void generate_multiline_fallback(irc_msg_vec *out, const struct irc_msg *msg, size_t max) {
	size_t start = out->len;
	multiline_line_vec lines = {0};
	split_multiline(&lines, str_from(msg->params.data[1]), max);
	for (size_t i = 0; i < lines.len; i++) {
		if (lines.data[i].text.len == 0) {
			continue;
		}
		struct irc_msg *m = irc_msg_clone(msg);
		AUTOFREE char *text = str_dup(lines.data[i].text);
		irc_msg_set_param(m, 1, text);
		if (out->len > start) {
			irc_tags_remove(&m->tags, "msgid");
		}
		vec_push(out, m);
	}
	vec_free(&lines);
	if (out->len == start) {
		struct irc_msg *m = irc_msg_clone(msg);
		irc_msg_set_param(m, 1, "");
		vec_push(out, m);
	}
}

const char *multiline_error_code(enum multiline_error e) {
	switch (e) {
	case MULTILINE_MAX_BYTES:
		return "MULTILINE_MAX_BYTES";
	case MULTILINE_MAX_LINES:
		return "MULTILINE_MAX_LINES";
	case MULTILINE_INVALID_TARGET:
		return "MULTILINE_INVALID_TARGET";
	case MULTILINE_INVALID:
		return "MULTILINE_INVALID";
	case MULTILINE_OK:
		break;
	}
	return "";
}

void multiline_batch_init(struct multiline_batch *b, struct irc_msg *opener) {
	const char *target = irc_msg_param(opener, 2);
	*b = (struct multiline_batch){
		.opener = opener,
		.target = xstrdup(target),
		.command = xstrdup(""),
		.text = STRBUF_INIT,
		.error = target[0] == '\0' ? MULTILINE_INVALID : MULTILINE_OK,
	};
}

void multiline_batch_free(struct multiline_batch *b) {
	irc_msg_free(b->opener);
	xfree(b->target);
	xfree(b->command);
	strbuf_free(&b->text);
	*b = (struct multiline_batch){0};
}

static enum multiline_error push_inner(struct multiline_batch *b, const struct irc_msg *msg,
				       const struct multiline_limits *limits) {
	if (!is_text_command(msg->command) ||
	    (b->command[0] != '\0' && !cstr_eq(msg->command, b->command)) || msg->params.len < 2) {
		return MULTILINE_INVALID;
	}
	if (!cstr_eq(msg->params.data[0], b->target)) {
		return MULTILINE_INVALID_TARGET;
	}
	const char *line = msg->params.data[1];
	bool concat = irc_tags_contains(&msg->tags, MULTILINE_CONCAT_TAG);
	if (concat && line[0] == '\0') {
		return MULTILINE_INVALID;
	}
	b->lines++;
	if (b->lines > limits->max_lines) {
		return MULTILINE_MAX_LINES;
	}
	b->bytes = size_add(b->bytes, strlen(line));
	if (b->bytes > limits->max_bytes) {
		return MULTILINE_MAX_BYTES;
	}
	if (b->lines > 1 && !concat) {
		strbuf_append_char(&b->text, '\n');
	}
	strbuf_append_cstr(&b->text, line);
	char *cmd = xstrdup(msg->command);
	xfree(b->command);
	b->command = cmd;
	return MULTILINE_OK;
}

void multiline_batch_push(struct multiline_batch *b, const struct irc_msg *msg,
			  const struct multiline_limits *limits) {
	if (b->error != MULTILINE_OK) {
		return;
	}
	enum multiline_error e = push_inner(b, msg, limits);
	if (e != MULTILINE_OK) {
		b->error = e;
		strbuf_reset(&b->text);
	}
}

struct irc_msg *multiline_batch_finish(struct multiline_batch *b, enum multiline_error *error) {
	struct irc_msg *msg = nullptr;
	*error = b->error;
	if (b->error == MULTILINE_OK) {
		// Trailing blank lines carry no meaning, and wouldn't survive
		// being stored as a raw IRC line
		size_t len = b->text.len;
		while (len > 0 && b->text.buf[len - 1] == '\n') {
			len--;
		}
		if (len == 0) {
			*error = MULTILINE_INVALID;
		} else {
			strbuf_truncate(&b->text, len);
			msg = IRC_MSG(b->command, b->target, strbuf_cstr(&b->text));
			irc_tags_copy(&msg->tags, &b->opener->tags);
			if (b->opener->prefix != nullptr) {
				irc_msg_set_prefix_from(msg, b->opener->prefix);
			}
		}
	}
	multiline_batch_free(b);
	return msg;
}
