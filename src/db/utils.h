// soju-rs database/utils.rs
#pragma once

#include "base/str.h"

// Strips IRC formatting characters from a string (appended to out)
void strip_ansi(struct strbuf *out, struct str s);
