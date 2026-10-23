// C23 public-header and link smoke test for every dependency.
#include <setjmp.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdio.h>

#include <cmocka.h>

#include "main/buildinfo.h"

int main(void) {
	cmocka_set_message_output(CM_OUTPUT_TAP);
	print_build_info(stdout);
	return 0;
}
