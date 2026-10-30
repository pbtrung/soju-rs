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
	VEC(irc_msg_vec) target_metadata;
	bool metadata_updates_unbatched;
	bool send_networks;
	char *batch_type;
	strvec batch_params;
	// The command's label, once taken by an upstream command
	char *label;
	// The upstream labeled batch the messages belong to, if any
	char *batch_tag;
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
void user_forward_channel(struct user *u, struct user_network *net, const struct user_client *dc,
			  struct user_packet *p, const struct upstream_channel *ch);
void user_names_reply(const struct user_client *dc, struct user_packet *p,
		      const struct upstream_channel *ch);
// extended: with the extended-join parameters (soju-rs sends them on
// welcome, not on reattach)
void user_channel_replay(struct user *u, struct user_network *net, const struct user_client *dc,
			 struct user_packet *p, const struct upstream_channel *ch, bool extended);
void user_history(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		  struct user_packet *reply);
void user_search(struct user *u, struct user_client *dc, const struct irc_msg *msg,
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
void user_upstream_channel_complete(struct upstream *uc, const char *channel, void *data);
void user_network_start(struct user *u, struct user_network *net);
void user_network_free(struct user_network *net);
void user_client_free(struct user_client *dc);
void user_send_networks(struct user *u, struct user_packet *p);
void user_save_delivered(struct user *u, struct user_network *net, const char *client);
void user_produce(struct user *u, struct user_network *net, struct irc_msg *msg, const char *target,
		  uint64_t origin_id, uint64_t reply_id);

// Web Push (user/webpush.c, soju-rs user/webpush.rs)
void user_web_push_init(struct user *u);
// Cancels the delayed notifications and frees the Web Push state
void user_web_push_stop(struct user *u);
// Whether a network (0: the bouncer connection) may have subscriptions
[[nodiscard]] bool user_has_web_push_subscriptions(struct user *u, int64_t net_id);
void user_reset_web_push_networks(struct user *u);
// Sends msg (taken) to a network's subscriptions, in the background
void user_broadcast_web_push(struct user *u, int64_t net_id, struct irc_msg *msg);
// A message from upstream: pushed after 5 s unless the buffer was read
// meanwhile, if it's a highlight or a direct message
void user_web_push_message(struct user *u, struct user_network *net, const struct irc_msg *msg,
			   const char *buffer, bool direct, bool self_msg);
// A read marker was set on target: dismisses its notification
void user_web_push_read(struct user *u, struct user_network *net, const char *target,
			const char *bound, struct db_time receipt);
// WEBPUSH (user/webpush_cmd.c): returns true when the reply comes later
// (the command must not be acknowledged now)
[[nodiscard]] bool user_webpush_command(struct user *u, struct user_client *dc,
					const struct irc_msg *msg, struct user_packet *p);

// soju.im/unread (user/mention.c): records the latest mention of a
// conversation, stored in the background with the others of the period
void user_record_mention(struct user *u, int64_t net_id, const char *target_cm,
			 struct timestamp time);
// Stores the mentions waiting to be stored
void user_mentions_stop(struct user *u);
// Cancels a pending retry of the user's loading
void user_load_retry_stop(struct user *u);

// soju-rs auto_save_sasl_plain
void user_auto_save_sasl_plain(struct user *u, struct user_network *net, const char *username,
			       const char *password);

// CLIENTCERT (user/clientcert.c)
void user_clientcert(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		     struct user_packet *p);

void user_metadata(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		   struct user_packet *p);

void user_initial_metadata(struct user *u, const struct user_client *dc, struct user_packet *p);
