// soju, a user-friendly IRC bouncer. The server itself is ported in later
// phases (PLAN.md section 5); for now only -version and -build-info work.
#include <stdio.h>
#include <string.h>

#include "main/buildinfo.h"
#include "version.h"

int main(int argc, char *argv[]) {
	for (int i = 1; i < argc; i++) {
		const char *a = argv[i];
		if (strcmp(a, "-version") == 0 || strcmp(a, "--version") == 0) {
			printf("soju %s\n", SOJU_VERSION);
			return 0;
		}
		if (strcmp(a, "-build-info") == 0 || strcmp(a, "--build-info") == 0) {
			print_build_info(stdout);
			return 0;
		}
	}
	fprintf(stderr, "soju: the server isn't implemented yet\n");
	return 1;
}
