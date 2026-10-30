#include "server/admin.h"

#include "base/alloc.h"
#include "config/flags.h"
#include "service/service.h"

struct admin {
	struct ref_node ref;
	struct server *srv;
	struct conn *conn;
	strvec commands;
	char *error;
	unsigned refs;
	bool busy, closed;
};

static void unref(struct admin *admin) {
	if (--admin->refs != 0) {
		return;
	}
	strvec_free(&admin->commands);
	xfree(admin->error);
	conn_unref(admin->conn);
	ref_untrack(&admin->ref);
	xfree(admin);
}

static void send_reply(struct admin *admin, struct irc_msg *msg) {
	server_set_prefix(admin->srv, msg);
	conn_send(admin->conn, msg);
	irc_msg_free(msg);
}
static void print_line(void *data, const char *line) {
	struct admin *admin = data;
	if (str_starts_with(str_from(line), STR_LIT("error: "))) {
		xfree(admin->error);
		admin->error = xstrdup(line + strlen("error: "));
	} else if (!admin->closed) {
		send_reply(admin, IRC_MSG("PRIVMSG", "*", line));
	}
}
static void start_next(struct admin *admin);
static void command_done(void *data) {
	struct admin *admin = data;
	if (!admin->closed) {
		send_reply(admin, admin->error != nullptr
					  ? IRC_MSG("FAIL", "BOUNCERSERV", admin->error)
					  : IRC_MSG("BOUNCERSERV", "OK"));
		admin->busy = false;
		conn_input_done(admin->conn);
		start_next(admin);
	}
	unref(admin);
}
static void start_next(struct admin *admin) {
	if (admin->busy || admin->closed || admin->commands.len == 0) {
		return;
	}
	admin->busy = true;
	admin->refs++;
	xfree(admin->error);
	admin->error = nullptr;
	AUTOFREE char *text = admin->commands.data[0];
	vec_remove(&admin->commands, 0);
	service_global(admin->srv, true, text, print_line, command_done, admin);
}
static void closed(struct conn *conn, const struct err *error, void *data) {
	(void)conn;
	(void)error;
	struct admin *admin = data;
	admin->closed = true;
	unref(admin);
}
static void message(struct conn *conn, struct irc_msg *msg, void *data) {
	struct admin *admin = data;
	admin->refs++;
	bool complete = true;
	if (cstr_eq(msg->command, "CAP") || cstr_eq(msg->command, "NICK") ||
	    cstr_eq(msg->command, "USER") || cstr_eq(msg->command, "PASS")) {
		send_reply(admin, IRC_MSG("ERROR", "This is not a regular IRC server"));
		conn_shutdown(conn);
	} else if (cstr_eq(msg->command, "BOUNCERSERV")) {
		if (msg->params.len == 0) {
			send_reply(admin,
				   IRC_MSG("461", "*", msg->command, "Not enough parameters"));
		} else {
			strvec_push(&admin->commands, msg->params.data[0]);
			complete = false;
			start_next(admin);
		}
	} else {
		send_reply(admin, IRC_MSG("421", "*", msg->command, "Unknown command"));
	}
	irc_msg_free(msg);
	if (complete) {
		conn_input_done(conn);
	}
	unref(admin);
}
void admin_serve(struct server *srv, struct conn *conn) {
	struct admin *admin = xcalloc(1, sizeof(*admin));
	REF_TRACK(&admin->ref, "admin");
	admin->srv = srv;
	admin->conn = conn;
	admin->refs = 1;
	conn_set_callbacks(conn, &(struct conn_callbacks){.message = message, .closed = closed},
			   admin);
	if (atomic_load(&srv->shutting_down)) {
		conn_close(conn);
	}
}

bool admin_quote_words(const char *const *words, size_t count, struct strbuf *out,
		       struct err **err) {
	for (size_t i = 0; i < count; i++) {
		if (strchr(words[i], '\r') != nullptr || strchr(words[i], '\n') != nullptr) {
			return err_set(err, 0, "arguments can't contain CR, LF or NUL characters");
		}
	}
	for (size_t i = 0; i < count; i++) {
		if (i > 0) {
			strbuf_append_char(out, ' ');
		}
		strbuf_append_char(out, '"');
		for (const char *p = words[i]; *p != '\0'; p++) {
			if (*p == '"' || *p == '\\') {
				strbuf_append_char(out, '\\');
			}
			strbuf_append_char(out, *p);
		}
		strbuf_append_char(out, '"');
	}
	return true;
}

bool admin_create_turso_admin(const struct config *config, const char *const *words, size_t count,
			      struct err **err) {
	AUTO_STRBUF struct strbuf checked = STRBUF_INIT;
	if (!admin_quote_words(words, count, &checked, err)) {
		return false;
	}
	if (!config->has_turso) {
		return err_set(err, 0, "turso create-admin requires message-store turso");
	}
	struct flag_set flags;
	flag_set_init(&flags);
	flag_set_add(&flags, "username", FLAG_STRING);
	flag_set_add(&flags, "password", FLAG_STRING);
	struct flags_parsed parsed;
	bool ok = flag_set_parse(&flags, words, count, &parsed, err);
	flag_set_free(&flags);
	if (!ok) {
		return false;
	}
	const char *username = flags_get(&parsed, "username");
	const char *password = flags_get(&parsed, "password");
	if (parsed.args.len != 0) {
		ok = err_set(err, 0, "unexpected positional arguments");
	} else if (username == nullptr || username[0] == '\0') {
		ok = err_set(err, 0, "flag -username is required");
	} else if (password == nullptr || password[0] == '\0') {
		ok = err_set(err, 0, "flag -password is required");
	} else {
		struct db_user user;
		db_user_init(&user, username);
		user.admin = true;
		struct db *db = nullptr;
		ok = db_user_set_password(&user, password, err) &&
		     db_open_turso(&config->turso, nullptr, nullptr, &db, err) &&
		     db_store_user(db, &user, err);
		if (db != nullptr) {
			db_close(db);
		}
		db_user_free(&user);
	}
	flags_parsed_free(&parsed);
	return ok;
}
