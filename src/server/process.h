#pragma once
#include "base/vec.h"
int server_run(const char *config_path, const strvec *extra_listeners, bool debug);
