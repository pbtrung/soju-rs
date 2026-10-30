// The admin client always talks to the running server, except for the
// explicit offline Turso bootstrap command.
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/un.h>
#include <unistd.h>

#include "base/alloc.h"
#include "config/flags.h"
#include "net/listen_uri.h"
#include "server/admin.h"

static constexpr char USAGE[] =
	"usage: sojuctl [-config path] <command>\n"
	"       sojuctl -config path turso create-admin -username name -password password\n";

static bool run_command(const struct config *config, const char *const *words, size_t count,
			struct err **err) {
	AUTO_STRBUF struct strbuf command = STRBUF_INIT;
	AUTO_STRBUF struct strbuf wire = STRBUF_INIT;
	AUTO_STRBUF struct strbuf pending = STRBUF_INIT;
	AUTOFREE char *line = nullptr;
	if (!admin_quote_words(words, count, &command, err)) {
		return false;
	}
	AUTOFREE char *path = nullptr;
	for (size_t i = 0; i < config->listen.len; i++) {
		struct listener_spec spec;
		if (!parse_listen_uri(config->listen.data[i], &spec, nullptr)) {
			continue;
		}
		if (spec.kind == LISTENER_ADMIN && path == nullptr) {
			path = xstrdup(spec.addr);
		}
		listener_spec_free(&spec);
	}
	if (path == nullptr) {
		return err_set(err, 0, "no listen unix+admin directive found in config");
	}
	struct sockaddr_un address = {.sun_family = AF_UNIX};
	if (strlen(path) >= sizeof(address.sun_path)) {
		return err_set(err, 0, "admin socket path is too long");
	}
	memcpy(address.sun_path, path, strlen(path) + 1);
	int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
	if (fd < 0) {
		return err_set(err, errno, "socket: %s", strerror(errno));
	}
	bool ok = false;
	struct timeval timeout = {.tv_sec = 60};
	setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
	setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout));
	if (connect(fd, (const struct sockaddr *)&address, sizeof(address)) < 0) {
		ok = err_set(err, errno, "dial %s: %s", path, strerror(errno));
		goto out;
	}
	struct irc_msg *msg = IRC_MSG("BOUNCERSERV", strbuf_cstr(&command));
	line = irc_msg_to_string(msg);
	irc_msg_free(msg);
	strbuf_printf(&wire, "%s\r\n", line);
	for (size_t sent = 0; sent < wire.len;) {
		ssize_t n = send(fd, wire.buf + sent, wire.len - sent, MSG_NOSIGNAL);
		if (n < 0 && errno == EINTR) {
			continue;
		}
		if (n <= 0) {
			ok = err_set(err, errno, "write: %s", strerror(errno));
			goto out;
		}
		sent += (size_t)n;
	}
	for (;;) {
		ptrdiff_t newline = str_index(strbuf_str(&pending), '\n');
		if (newline >= 0) {
			msg = irc_msg_parse(str_new(pending.buf, (size_t)newline), nullptr);
			strbuf_drop_front(&pending, (size_t)newline + 1);
			if (msg == nullptr) {
				continue;
			}
			if (cstr_eq(msg->command, "PRIVMSG")) {
				puts(irc_msg_trailing(msg));
			} else if (cstr_eq(msg->command, "BOUNCERSERV") &&
				   cstr_eq(irc_msg_param(msg, 0), "OK")) {
				irc_msg_free(msg);
				ok = true;
				goto out;
			} else {
				ok = err_set(err, 0, "%s", irc_msg_trailing(msg));
				irc_msg_free(msg);
				goto out;
			}
			irc_msg_free(msg);
			continue;
		}
		if (pending.len > MAX_LINE_LENGTH) {
			ok = err_set(err, 0, "admin reply is too long");
			goto out;
		}
		char block[4096];
		ssize_t n = recv(fd, block, sizeof(block), 0);
		if (n < 0 && errno == EINTR) {
			continue;
		}
		if (n <= 0) {
			ok = err_set(err, errno, "read: %s", n == 0 ? "EOF" : strerror(errno));
			goto out;
		}
		strbuf_append(&pending, block, (size_t)n);
	}
out:
	close(fd);
	return ok;
}

int main(int argc, char *argv[]) {
	signal(SIGPIPE, SIG_IGN);
	struct flag_set flags;
	flag_set_init(&flags);
	flag_set_add(&flags, "config", FLAG_STRING);
	struct flags_parsed parsed;
	struct err *error = nullptr;
	bool ok = flag_set_parse(&flags, (const char *const *)(argv + 1), (size_t)(argc - 1),
				 &parsed, &error);
	flag_set_free(&flags);
	if (!ok) {
		if (error->code != FLAG_ERR_HELP) {
			fprintf(stderr, "%s\n", err_msg(error));
		}
		fputs(USAGE, stderr);
		err_free(error);
		return 2;
	}
	struct config config;
	const char *path = flags_get(&parsed, "config");
	if (path == nullptr) {
		path = CONFIG_DEFAULT_PATH;
	}
	if (path[0] == '\0') {
		config_defaults(&config);
	} else if (!config_load(path, &config, &error)) {
		fprintf(stderr, "failed to load config file: %s\n", err_msg(error));
		err_free(error);
		flags_parsed_free(&parsed);
		return 1;
	}
	const char *const *words = (const char *const *)parsed.args.data;
	size_t count = parsed.args.len;
	if (count >= 2 && cstr_eq(words[0], "turso") && cstr_eq(words[1], "create-admin")) {
		ok = admin_create_turso_admin(&config, words + 2, count - 2, &error);
		if (ok) {
			puts("created admin and their Turso database; start or restart soju to "
			     "load the user");
		}
	} else {
		ok = run_command(&config, words, count, &error);
	}
	if (!ok) {
		fprintf(stderr, "%s\n", err_msg(error));
	}
	err_free(error);
	config_free(&config);
	flags_parsed_free(&parsed);
	return ok ? 0 : 1;
}
