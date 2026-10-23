#include "main/buildinfo.h"

#include <crypt.h>
#include <libwebsockets.h>
#include <openssl/crypto.h>
#include <sqlite3.h>
#include <utf8proc.h>
#include <uv.h>
#include <yyjson.h>

#ifdef SOJU_MIMALLOC
#include <mimalloc.h>
#endif

#include "version.h"

void print_build_info(FILE *f) {
	fprintf(f, "soju %s (%s build)\n", SOJU_VERSION, SOJU_BUILD_MODE);
	fprintf(f, "libuv %s\n", uv_version_string());
	fprintf(f, "%s\n", OpenSSL_version(OPENSSL_VERSION));
	fprintf(f, "libwebsockets %s\n", lws_get_library_version());
	fprintf(f, "utf8proc %s\n", utf8proc_version());
	fprintf(f, "yyjson %s\n", YYJSON_VERSION_STRING);
	fprintf(f, "SQLite %s\n", sqlite3_libversion());
	char salt[CRYPT_GENSALT_OUTPUT_SIZE];
	bool bcrypt = crypt_gensalt_rn("$2b$", 10, nullptr, 0, salt, sizeof(salt)) != nullptr;
	fprintf(f, "libxcrypt (bcrypt %s)\n", bcrypt ? "available" : "unavailable");
#ifdef SOJU_MIMALLOC
	int v = mi_version();
	fprintf(f, "mimalloc %d.%d.%d\n", v / 10000, v / 100 % 100, v % 100);
#else
	fprintf(f, "system malloc\n");
#endif
}
