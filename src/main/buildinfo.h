// Versions of the libraries linked into the binary (soju -build-info). Also
// proves at link time that every dependency is usable (PLAN.md phase 0).
#pragma once

#include <stdio.h>

void print_build_info(FILE *f);
