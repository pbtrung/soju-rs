#pragma once
#include "server/server.h"
struct maintenance;
// Startup expiry, then daily expiry and inactive-user checks, on loop zero.
[[nodiscard]] struct maintenance *maintenance_start(struct server *srv);
// Stops future work; in-flight DB jobs drain through server.pending_work.
void maintenance_stop(struct maintenance *maintenance);
