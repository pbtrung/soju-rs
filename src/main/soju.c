// soju process entry point; all asynchronous ownership lives in server/process.c.
#include <openssl/crypto.h>
#include <signal.h>
#include <stdio.h>
#include <uv.h>

#include "http/client.h"
#include "base/ref.h"
#include "config/flags.h"
#include "config/config.h"
#include "main/buildinfo.h"
#include "server/process.h"
#include "version.h"

static constexpr char USAGE[] = "Usage of soju:\n"
				"  -config string\n    \tpath to configuration file\n"
				"  -debug\n    \tenable debug logging\n"
				"  -listen value\n    \tlistening address (repeatable)\n"
				"  -version\n    \tprint the version and exit\n"
				"  -build-info\n    \tprint build and library versions\n";

int main(int argc, char *argv[]) {
	signal(SIGPIPE, SIG_IGN);
	signal(SIGHUP, SIG_IGN);
	struct flag_set flags;
	flag_set_init(&flags);
	flag_set_add(&flags, "config", FLAG_STRING);
	flag_set_add(&flags, "listen", FLAG_STRING);
	flag_set_add(&flags, "debug", FLAG_BOOL);
	flag_set_add(&flags, "version", FLAG_BOOL);
	flag_set_add(&flags, "build-info", FLAG_BOOL);
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
	bool version = false, build_info = false, debug = false;
	(void)flags_get_bool(&parsed, "version", &version);
	(void)flags_get_bool(&parsed, "build-info", &build_info);
	(void)flags_get_bool(&parsed, "debug", &debug);
	int status = 0;
	if (version) {
		printf("soju %s\n", SOJU_VERSION);
	} else if (build_info) {
		print_build_info(stdout);
	} else {
		const char *path = flags_get(&parsed, "config");
		strvec empty = {0};
		const strvec *listeners = flags_get_all(&parsed, "listen");
		status = server_run(path != nullptr ? path : CONFIG_DEFAULT_PATH,
				    listeners != nullptr ? listeners : &empty, debug);
	}
	flags_parsed_free(&parsed);
	uv_library_shutdown();
	http_client_global_cleanup();
	OPENSSL_cleanup();
	if (ref_dump_live(stderr) != 0) {
		return 70;
	}
	return status;
}
