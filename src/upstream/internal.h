#pragma once
#include "upstream/upstream.h"

void upstream_disconnect(struct upstream *uc, const char *reason, bool temporary);
void upstream_notify_state(struct upstream *uc);
void upstream_emit_take(struct upstream *uc, struct irc_msg *msg, uint64_t dc_id);
[[nodiscard]] bool upstream_handle_registration(struct upstream *uc, struct irc_msg *msg);
[[nodiscard]] bool upstream_handle_channel(struct upstream *uc, struct irc_msg *msg);
void upstream_route_reply(struct upstream *uc, struct irc_msg *msg, uint64_t *dc_id);
void upstream_abort_commands(struct upstream *uc);
void upstream_clear_session(struct upstream *uc);
void upstream_channels_init(struct upstream *uc);
void upstream_abort_take(struct upstream *uc, struct pending_command *p);
// The upstreams metric counts connection attempts (soju-rs run_conn)
void upstream_count(struct upstream *uc, bool running);
