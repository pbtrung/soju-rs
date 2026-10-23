// Replay driver for fuzz harnesses outside SOJU_MODE=fuzz: runs
// LLVMFuzzerTestOneInput on every file of the directories (or files) given
// as arguments, so committed seeds and crash reproducers are regression
// tests in every build mode, including GCC and valgrind.
#include <dirent.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

#include "base/alloc.h"
#include "base/vec.h"

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size);

static bool run_file(const char *path) {
	FILE *f = fopen(path, "rb");
	if (f == nullptr) {
		perror(path);
		return false;
	}
	VEC(uint8_t) buf = {0};
	uint8_t chunk[4096];
	for (;;) {
		size_t n = fread(chunk, 1, sizeof(chunk), f);
		vec_reserve(&buf, n);
		memcpy(buf.data + buf.len, chunk, n);
		buf.len += n;
		if (n < sizeof(chunk)) {
			break; // end of file or error, told apart below
		}
	}
	bool ok = !ferror(f);
	fclose(f);
	if (ok) {
		// Copy into an exact-size block so overreads hit the redzone
		uint8_t *exact = xmemdup(buf.data != nullptr ? buf.data : chunk, buf.len);
		LLVMFuzzerTestOneInput(exact, buf.len);
		xfree(exact);
	}
	vec_free(&buf);
	return ok;
}

static int cmp_names(const void *a, const void *b) {
	return strcmp(*(char *const *)a, *(char *const *)b);
}

int main(int argc, char *argv[]) {
	size_t count = 0;
	for (int i = 1; i < argc; i++) {
		struct stat st;
		if (stat(argv[i], &st) != 0) {
			perror(argv[i]);
			return 1;
		}
		if (!S_ISDIR(st.st_mode)) {
			if (!run_file(argv[i])) {
				return 1;
			}
			count++;
			continue;
		}
		DIR *d = opendir(argv[i]);
		if (d == nullptr) {
			perror(argv[i]);
			return 1;
		}
		strvec paths = {0};
		struct dirent *de;
		while ((de = readdir(d)) != nullptr) {
			if (de->d_name[0] != '.') {
				strvec_push_take(&paths, xasprintf("%s/%s", argv[i], de->d_name));
			}
		}
		closedir(d);
		if (paths.len > 1) {
			qsort(paths.data, paths.len, sizeof(*paths.data), cmp_names);
		}
		for (size_t j = 0; j < paths.len; j++) {
			if (!run_file(paths.data[j])) {
				strvec_free(&paths);
				return 1;
			}
			count++;
		}
		strvec_free(&paths);
	}
	printf("replayed %zu inputs\n", count);
	return 0;
}
