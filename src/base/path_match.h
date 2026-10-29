// Shell pattern matching with the semantics of Go's path.Match (soju-rs
// util.rs path_match)
#pragma once

#include "base/str.h"

// Sets *matched; returns false on a malformed pattern ("syntax error in
// pattern")
[[nodiscard]] bool path_match(struct str pattern, struct str name, bool *matched);
