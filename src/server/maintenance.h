#pragma once
#include "server/server.h"
struct maintenance;
// Deletes expired messages and vacuums local SQLite, before users start and
// clients can connect: rewriting the database holds the DB thread, which
// user loops wait on. Blocks the calling loop meanwhile.
void maintenance_startup(struct server *srv);
// Daily expiry (without vacuum: the space freed each day is reused by the
// next day's messages) and inactive-user checks, on loop zero.
[[nodiscard]] struct maintenance *maintenance_start(struct server *srv);
// The next expiry vacuums local SQLite, now if idle: message-expiry
// changed, which may leave much of the database free
void maintenance_request_vacuum(struct maintenance *maintenance);
// Stops future work; in-flight DB jobs drain through server.pending_work.
void maintenance_stop(struct maintenance *maintenance);
