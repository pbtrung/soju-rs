#include "service/internal.h"
#include "irc/ircutil.h"
#include <openssl/evp.h>
#include <stdlib.h>

static const struct service_command *lookup(const struct service_command *cmds, size_t len,
					    const char *const *words, size_t n, size_t *used,
					    struct err **err) {
	if (n == 0) {
		bool ok = err_set(err, 0, "no command specified");
		(void)ok;
		return nullptr;
	}
	const struct service_command *found = nullptr;
	for (size_t i = 0; i < len; i++) {
		if (cstr_eq(cmds[i].name, words[0])) {
			found = &cmds[i];
			break;
		}
	}
	if (found == nullptr) {
		for (size_t i = 0; i < len; i++) {
			if (!str_starts_with(str_from(cmds[i].name), str_from(words[0]))) {
				continue;
			}
			if (found != nullptr) {
				AUTOFREE char *q = utf8_debug_cstr(str_from(words[0]));
				bool ok = err_set(err, 0, "command %s is ambiguous", q);
				(void)ok;
				return nullptr;
			}
			found = &cmds[i];
		}
	}
	if (found == nullptr) {
		AUTOFREE char *q = utf8_debug_cstr(str_from(words[0]));
		bool ok = err_set(err, 0, "command %s not found", q);
		(void)ok;
		return nullptr;
	}
	(*used)++;
	if (found->children_len > 0 && n > 1) {
		return lookup(found->children, found->children_len, words + 1, n - 1, used, err);
	}
	return found;
}
const struct service_command *service_lookup(const char *const *words, size_t n, size_t *used,
					     struct err **err) {
	*used = 0;
	return lookup(service_commands, service_commands_len, words, n, used, err);
}
static int compare_commands(const void *a, const void *b) {
	const struct service_command *const *x = a, *const *y = b;
	return strcmp((*x)->name, (*y)->name);
}
static void append_help(const struct service_command *cmds, size_t len, const char *prefix,
			bool admin, bool global, strvec *out) {
	const struct service_command **sorted = xmalloc(len * sizeof(*sorted));
	for (size_t i = 0; i < len; i++) {
		sorted[i] = &cmds[i];
	}
	qsort(sorted, len, sizeof(*sorted), compare_commands);
	for (size_t i = 0; i < len; i++) {
		const struct service_command *cmd = sorted[i];
		if ((cmd->admin && !admin) || (!cmd->global && global)) {
			continue;
		}
		AUTOFREE char *name = xasprintf("%s%s%s", prefix, prefix[0] ? " " : "", cmd->name);
		if (cmd->children_len > 0) {
			append_help(cmd->children, cmd->children_len, name, admin, global, out);
		} else {
			strvec_push(out, name);
		}
	}
	xfree(sorted);
}
void service_help(bool admin, bool global, strvec *out) {
	append_help(service_commands, service_commands_len, "", admin, global, out);
}
static void print_listing(struct service_context *c, const struct service_command *cmds, size_t len,
			  const char *prefix, bool global) {
	strvec lines = {0};
	append_help(cmds, len, prefix, c->admin, global, &lines);
	AUTO_STRBUF struct strbuf out = {0};
	strbuf_append_cstr(&out, "available commands: ");
	for (size_t i = 0; i < lines.len; i++) {
		if (i) {
			strbuf_append_cstr(&out, ", ");
		}
		strbuf_append_cstr(&out, lines.data[i]);
	}
	service_print(c, strbuf_cstr(&out));
	strvec_free(&lines);
}
HANDLER(handle_help) {
	if (n == 0) {
		print_listing(c, service_commands, service_commands_len, "", u == nullptr);
		return true;
	}
	size_t used = 0;
	const struct service_command *cmd = service_lookup(args, n, &used, &c->err);
	if (cmd == nullptr) {
		return false;
	}
	AUTO_STRBUF struct strbuf name = {0};
	for (size_t i = 0; i < used; i++) {
		if (i) {
			strbuf_append_char(&name, ' ');
		}
		strbuf_append_cstr(&name, args[i]);
	}
	if (cmd->children_len > 0) {
		print_listing(c, cmd->children, cmd->children_len, strbuf_cstr(&name),
			      u == nullptr);
	} else {
		service_printf(c, "%s%s%s: %s", strbuf_cstr(&name), cmd->usage[0] ? " " : "",
			       cmd->usage, cmd->desc);
	}
	return true;
}
struct output {
	uint64_t dc;
	char *line;
};
static void output_drop(void *data) {
	struct output *o = data;
	xfree(o->line);
	xfree(o);
}
static void output_run(struct user *u, void *data) {
	struct output *o = data;
	user_service_reply(u, o->dc, o->line);
	output_drop(o);
}
struct callback_output {
	struct server *srv;
	service_print_fn print;
	service_done_fn done;
	void *data;
	char *line;
};
static void callback_drop(void *data) {
	struct callback_output *o = data;
	xfree(o->line);
	atomic_fetch_sub(&o->srv->pending_work, 1);
	xfree(o);
}
static void callback_run(void *data) {
	struct callback_output *o = data;
	if (o->print != nullptr) {
		o->print(o->data, o->line);
	}
	if (o->done != nullptr) {
		o->done(o->data);
	}
	callback_drop(o);
}
static void post_callback(struct service_context *c, const char *text, bool done) {
	struct callback_output *o = xcalloc(1, sizeof(*o));
	*o = (struct callback_output){.srv = c->srv,
				      .print = done ? nullptr : c->print,
				      .done = done ? c->done : nullptr,
				      .data = c->data,
				      .line = text != nullptr ? xstrdup(text) : nullptr};
	atomic_fetch_add(&c->srv->pending_work, 1);
	loop_post(c->origin, callback_run, callback_drop, o);
}
void service_print(struct service_context *c, const char *text) {
	if (loop_current() == c->origin && c->packet != nullptr) {
		struct irc_msg *msg = IRC_MSG("PRIVMSG", c->nick, text);
		irc_msg_set_prefix(msg, SERVICE_NICK, SERVICE_NICK, SERVICE_NICK);
		user_packet_add_take(c->packet, msg);
	} else if (c->print != nullptr) {
		post_callback(c, text, false);
	} else {
		struct output *o = xmalloc(sizeof(*o));
		*o = (struct output){.dc = c->dc_id, .line = xstrdup(text)};
		user_post(c->srv, c->reply_user, output_run, output_drop, o);
	}
}
void service_printf(struct service_context *c, const char *fmt, ...) {
	va_list ap;
	va_start(ap, fmt);
	AUTO_STRBUF struct strbuf b = {0};
	strbuf_vprintf(&b, fmt, ap);
	va_end(ap);
	service_print(c, strbuf_cstr(&b));
}
static void context_unref(struct service_context *c) {
	if (atomic_fetch_sub(&c->refs, 1) != 1) {
		return;
	}
	err_free(c->err);
	xfree(c->nick);
	xfree(c->command);
	strbuf_free(&c->peer_cert);
	atomic_fetch_sub(&c->srv->pending_work, 1);
	xfree(c);
}
void service_defer(struct service_context *c) {
	atomic_fetch_add(&c->refs, 1);
}
void service_finish(struct service_context *c) {
	if (c->err != nullptr) {
		service_printf(c, "error: %s", err_msg(c->err));
	}
	if (c->done != nullptr) {
		post_callback(c, nullptr, true);
	}
	context_unref(c);
}
void service_drop(void *data) {
	struct service_context *c = data;
	bool ok = SFAIL(c, "user stopped");
	(void)ok;
	service_finish(c);
}
void service_run(struct user *u, struct service_context *c, const char *const *words, size_t n) {
	if (atomic_load(&c->srv->shutting_down)) {
		bool ok = SFAIL(c, "server shutting down");
		(void)ok;
		service_finish(c);
		return;
	}
	if (c->depth++ >= 32 || uv_hrtime() > c->deadline) {
		bool ok = SFAIL(c, "context deadline exceeded");
		(void)ok;
		service_finish(c);
		return;
	}
	size_t used = 0;
	const struct service_command *cmd = service_lookup(words, n, &used, &c->err);
	if (cmd == nullptr) {
		AUTOFREE char *message = xstrdup(err_msg(c->err));
		err_free(c->err);
		c->err = nullptr;
		bool ok = SFAIL(c, "%s (type \"help\" for a list of commands)", message);
		(void)ok;
		service_finish(c);
		return;
	}
	AUTO_STRBUF struct strbuf path = {0};
	for (size_t i = 0; i < used; i++) {
		if (i) {
			strbuf_append_char(&path, ' ');
		}
		strbuf_append_cstr(&path, words[i]);
	}
	service_replace(&c->command, strbuf_cstr(&path));
	int ok = true;
	if (cmd->admin && !c->admin) {
		ok = SFAIL(c, "you must be an admin to use this command");
	} else if (!cmd->global && u == nullptr) {
		ok = SFAIL(c, "this command must be run as a user (try running with user run)");
	} else if (cmd->handle == nullptr) {
		AUTO_STRBUF struct strbuf prefix = {0};
		for (size_t i = 0; i < used; i++) {
			if (i) {
				strbuf_append_char(&prefix, ' ');
			}
			strbuf_append_cstr(&prefix, words[i]);
		}
		print_listing(c, cmd->children, cmd->children_len, strbuf_cstr(&prefix),
			      u == nullptr);
	} else {
		ok = cmd->handle(u, c, words + used, n - used);
	}
	if (ok == SERVICE_DEFERRED) {
		context_unref(c);
		return;
	}
	if (!ok && c->err == nullptr) {
		bool failed = SFAIL(c, "command failed");
		(void)failed;
	}
	service_finish(c);
}
static struct service_context *context_new(struct server *srv, bool admin) {
	struct service_context *c = xcalloc(1, sizeof(*c));
	c->srv = srv;
	atomic_init(&c->refs, 1);
	c->admin = admin;
	c->nick = xstrdup("");
	c->origin = loop_current();
	c->deadline = uv_hrtime() + 10000000000ULL;
	atomic_fetch_add(&srv->pending_work, 1);
	return c;
}
static void run_text(struct user *u, struct service_context *c, const char *text) {
	strvec words = {0};
	if (!split_words(str_from(text), &words, &c->err)) {
		bool ok = err_wrap(&c->err, "failed to parse command");
		(void)ok;
		service_finish(c);
	} else {
		service_run(u, c, (const char *const *)words.data, words.len);
	}
	strvec_free(&words);
}
void service_privmsg(struct user *u, const struct user_client *dc, const char *text,
		     struct user_packet *p) {
	struct service_context *c = context_new(u->srv, u->record.admin && !dc->impersonating);
	c->packet = p;
	c->user_id = u->record.id;
	c->reply_user = u->record.id;
	c->dc_id = dc->id;
	c->network_id = dc->network_id;
	service_replace(&c->nick, dc->nick);
	strbuf_append_str(&c->peer_cert, strbuf_str(&dc->peer_cert));
	c->has_remote_ip = dc->has_remote_ip;
	c->remote_ip = dc->remote_ip;
	service_defer(c);
	run_text(u, c, text);
	// Later output is sent on its own
	c->packet = nullptr;
	context_unref(c);
}
void service_global(struct server *srv, bool admin, const char *text, service_print_fn print,
		    service_done_fn done, void *data) {
	struct service_context *c = context_new(srv, admin);
	c->print = print;
	c->done = done;
	c->data = data;
	run_text(nullptr, c, text);
}
void service_replace(char **dst, const char *value) {
	if (value != nullptr) {
		char *s = xstrdup(value);
		xfree(*dst);
		*dst = s;
	}
}
bool service_parse(struct service_context *c, struct flags_parsed *p, const char *spec,
		   const char *const *args, size_t n) {
	struct flag_set fs;
	flag_set_init(&fs);
	// Specs are space separated name:type, with s=string,b=bool,v=bool value,i=int,a=Atoi.
	struct str rest = str_from(spec), part;
	while (str_next_field(&rest, ' ', &part)) {
		if (part.len < 3) {
			continue;
		}
		char kind = part.p[part.len - 1];
		AUTOFREE char *name = str_dup(str_slice(part, 0, part.len - 2));
		flag_set_add(&fs, name,
			     kind == 'b'   ? FLAG_BOOL
			     : kind == 'v' ? FLAG_BOOL_VALUE
			     : kind == 'i' ? FLAG_INT
			     : kind == 'a' ? FLAG_ATOI
					   : FLAG_STRING);
	}
	bool ok = flag_set_parse(&fs, args, n, p, &c->err);
	flag_set_free(&fs);
	if (!ok && c->err->code == FLAG_ERR_HELP) {
		err_free(c->err);
		c->err = nullptr;
		return SFAIL(c, "unsupported flag (type \"help %s\" for a help message)",
			     c->command);
	}
	return ok;
}
bool service_no_args(struct service_context *c, const struct flags_parsed *p) {
	return p->args.len == 0 || SFAIL(c, "unexpected argument: %s", p->args.data[0]);
}
struct user_network *service_network(struct user *u, struct service_context *c, const char *name) {
	struct user_network *net = name != nullptr && name[0] ? user_find_network(u, name)
							      : user_get_network(u, c->network_id);
	if (net == nullptr) {
		bool ok = name != nullptr && name[0] ? SFAIL(c, "unknown network \"%s\"", name)
						     : SFAIL(c, "no network selected, a name "
								"argument is required");
		(void)ok;
	}
	return net;
}
char *service_hex(struct str bytes, bool colon) {
	AUTO_STRBUF struct strbuf out = {0};
	for (size_t i = 0; i < bytes.len; i++) {
		if (i && colon) {
			strbuf_append_char(&out, ':');
		}
		strbuf_printf(&out, colon ? "%02X" : "%02x", (unsigned char)bytes.p[i]);
	}
	return strbuf_take(&out);
}
static int unhex(char c) {
	if (c >= '0' && c <= '9') {
		return c - '0';
	}
	if (c >= 'a' && c <= 'f') {
		return c - 'a' + 10;
	}
	if (c >= 'A' && c <= 'F') {
		return c - 'A' + 10;
	}
	return -1;
}
bool service_unhex(const char *s, struct strbuf *out) {
	int hi = -1;
	for (; *s; s++) {
		if (*s == ':') {
			continue;
		}
		int v = unhex(*s);
		if (v < 0) {
			return false;
		}
		if (hi < 0) {
			hi = v;
		} else {
			strbuf_append_char(out, (char)(hi * 16 + v));
			hi = -1;
		}
	}
	return hi < 0;
}
char *service_digest(const char *name, struct str bytes) {
	unsigned char digest[EVP_MAX_MD_SIZE];
	unsigned len = 0;
	if (EVP_Digest(bytes.p, bytes.len, digest, &len, EVP_get_digestbyname(name), nullptr) !=
	    1) {
		return xstrdup("");
	}
	return service_hex(str_new((const char *)digest, len), false);
}
