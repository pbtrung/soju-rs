// Ported from soju-rs src/database/utils.rs tests
#include "testsupport.h"

#include "db/utils.h"

static void strip_eq(const char *in, const char *want) {
	struct strbuf sb = STRBUF_INIT;
	strip_ansi(&sb, str_from(in));
	assert_sb_eq(&sb, want);
	strbuf_free(&sb);
}

static void test_strip(void **state) {
	(void)state;
	strip_eq("plain", "plain");
	strip_eq("\x02"
		 "bold\x02 \x03"
		 "04red\x03",
		 "bold red");
	strip_eq("\x03"
		 "04,12x\x03"
		 "1,2y",
		 "xy");
	strip_eq("\x03,5z", ",5z");
	strip_eq("\x04"
		 "ff00ff,00ff00hex\x04",
		 "hex");
	strip_eq("\x04nothex", "nothex");
	strip_eq("\xC3\xA9\x1D\xC3\xA0", "\xC3\xA9\xC3\xA0");
}

T_MAIN(cmocka_unit_test(test_strip))
