#pragma once
#include "user/user.h"

struct user_packet {
	struct server *srv;
	int64_t history_user;
	size_t history_count, queued;
	struct sloop *loop;
	uint64_t id, ack_id;
	struct irc_msg *command;
	irc_msg_vec messages;
	irc_msg_vec network_messages;
	bool send_networks;
	char *batch_type;
	strvec batch_params;
	bool welcome, sync, close, attach, detach, ack;
	int64_t network_id;
	struct downstream_sync state;
	char *state_strings[7];
	struct cap_registry caps;
	struct isupport isupport;
};

[[nodiscard]] struct user_packet *user_packet_new(const struct user_client *dc,
						  const struct irc_msg *command);
void user_packet_send_take(struct user_packet *p);
void user_packet_free(void *data);
void user_packet_add_take(struct user_packet *p, struct irc_msg *msg);
void user_packet_sync(struct user *u, struct user_network *net, struct user_packet *p,
		      bool welcome);
void user_channel_replay(struct user *u, struct user_network *net, const struct user_client *dc,
			 struct user_packet *p, const struct upstream_channel *ch);
void user_history(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		  struct user_packet *reply);
void user_read_marker(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		      struct user_packet *reply);
void user_handle_command(struct user *u, struct user_client *dc, struct irc_msg *msg,
			 uint64_t ack_id);
void user_update_away_monitor(struct user *u, struct user_network *net);
void user_upstream_message(struct upstream *uc, struct irc_msg *msg, uint64_t dc_id, void *data);
void user_upstream_state(struct upstream *uc, void *data);
void user_upstream_closed(struct upstream *uc, const char *reason, bool temporary, void *data);
void user_upstream_detach(struct upstream *uc, const char *channel, void *data);
void user_network_start(struct user *u, struct user_network *net);
void user_network_free(struct user_network *net);
void user_client_free(struct user_client *dc);
void user_send_networks(struct user *u, struct user_packet *p);
void user_save_delivered(struct user *u, struct user_network *net, const char *client);
void user_produce(struct user *u, struct user_network *net, struct irc_msg *msg, const char *target,
		  uint64_t origin_id, uint64_t reply_id);
