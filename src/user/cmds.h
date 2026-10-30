// Declarations shared by the command handlers of registered clients
// (user/commands.c and user/cmd_*.c): not part of user/internal.h
#pragma once
#include "user/internal.h"

void cmd_authenticate(struct user_client *dc, struct user_network *net, const struct irc_msg *msg,
		      struct user_packet *p);
enum casemapping cmd_client_casemap(const struct user_network *net);
void cmd_drop_label(struct user_packet *p);
bool cmd_enabled(const struct user_client *dc, const char *cap);
void cmd_fail(struct user_packet *p, const char *cmd, const char *code, const char *sub,
	      const char *text);
void cmd_handle_join(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		     struct user_packet *p);
void cmd_handle_mode(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		     struct user_packet *p);
void cmd_handle_names(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		      struct user_packet *p);
void cmd_handle_nick(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		     struct user_packet *p);
void cmd_handle_part(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		     struct user_packet *p);
void cmd_handle_setname(struct user *u, struct user_client *dc, const struct irc_msg *msg,
			struct user_packet *p);
void cmd_handle_topic(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		      struct user_packet *p);
void cmd_handle_who(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		    struct user_packet *p);
bool cmd_local_query(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		     struct user_packet *p);
void cmd_monitor(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		 struct user_packet *p);
bool cmd_need_params(const struct irc_msg *msg, size_t n, struct user_packet *p);
void cmd_no_ack(struct user_packet *p);
void cmd_privmsg(struct user *u, struct user_client *dc, const struct irc_msg *msg,
		 struct user_packet *p);
bool cmd_same_name(enum casemapping cm, const char *a, const char *b);
void cmd_send_labeled(struct upstream *uc, const struct user_client *dc, struct user_packet *p,
		      struct irc_msg *msg);
void cmd_split_list(const char *list, strvec *out);
void cmd_sync_bouncer_clients(struct user *u, bool nick_only);
const char *cmd_take_label(struct user_packet *p);
struct upstream *cmd_upstream_for(struct user_network *net, struct user_packet *p, const char *cmd);
