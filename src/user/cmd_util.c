// Helpers shared by the command handlers (soju-rs downstream/mod.rs)
#include "user/cmds.h"

#include "service/service.h"

#include "base/alloc.h"
#include "base/base64.h"
#include "irc/genmsg.h"
#include "irc/numerics.h"
#include "irc/whox.h"

void cmd_fail(struct user_packet *p, const char *cmd, const char *code, const char *sub,
	      const char *text) {
	user_packet_add_take(p, IRC_MSG("FAIL", cmd, code, sub, text));
}
bool cmd_enabled(const struct user_client *dc, const char *cap) {
	return cap_registry_is_enabled(&dc->caps, cap);
}
void cmd_no_ack(struct user_packet *p) {
	irc_msg_free(p->command);
	p->command = nullptr;
}

// The label is only used once: by the first upstream command, or the reply
const char *cmd_take_label(struct user_packet *p) {
	if (p->command == nullptr) {
		return "";
	}
	const char *label = irc_tags_get(&p->command->tags, "label");
	if (label != nullptr) {
		xfree(p->label);
		p->label = xstrdup(label);
		irc_tags_remove(&p->command->tags, "label");
		return p->label;
	}
	return "";
}
// For commands mixing local and remote replies (soju-rs drop_label)
void cmd_drop_label(struct user_packet *p) {
	if (p->command != nullptr) {
		irc_tags_remove(&p->command->tags, "label");
	}
}
enum casemapping cmd_client_casemap(const struct user_network *net) {
	return net == nullptr             ? CASEMAPPING_ASCII
	       : net->upstream != nullptr ? net->upstream->casemap
					  : CASEMAPPING_RFC1459;
}
bool cmd_same_name(enum casemapping cm, const char *a, const char *b) {
	AUTOFREE char *ca = casemap_cstr(cm, a);
	AUTOFREE char *cb = casemap_cstr(cm, b);
	return cstr_eq(ca, cb);
}
// soju-rs upstream_for_command: replies with an error if there's no
// upstream connection to send the command to
struct upstream *cmd_upstream_for(struct user_network *net, struct user_packet *p,
				  const char *cmd) {
	if (net == nullptr) {
		user_packet_add_take(
			p, IRC_MSG("400", "*", cmd,
				   "Cannot interact with channels and users on the bouncer "
				   "connection. Did you mean to use a specific network?"));
		return nullptr;
	}
	if (net->upstream == nullptr || !net->upstream->attached) {
		user_packet_add_take(
			p, IRC_MSG("400", "*", cmd, "Disconnected from upstream network"));
		return nullptr;
	}
	return net->upstream;
}
bool cmd_need_params(const struct irc_msg *msg, size_t n, struct user_packet *p) {
	if (msg->params.len >= n) {
		return true;
	}
	user_packet_add_take(p, IRC_MSG("461", "*", msg->command, "Not enough parameters"));
	return false;
}
void cmd_send_labeled(struct upstream *uc, const struct user_client *dc, struct user_packet *p,
		      struct irc_msg *msg) {
	upstream_send_labeled_take(uc, dc->id, cmd_take_label(p), msg);
}
// Sends a state update to the clients of the bouncer connection
void cmd_sync_bouncer_clients(struct user *u, bool nick_only) {
	for (size_t i = 0; i < u->clients.len; i++) {
		struct user_client *peer = u->clients.data[i];
		if (peer->network_id == 0) {
			struct user_packet *sync = user_packet_new(peer, nullptr);
			user_packet_sync(u, nullptr, sync, false);
			sync->state.nick_only = nick_only;
			user_packet_send_take(sync);
		}
	}
}

void cmd_split_list(const char *list, strvec *out) {
	struct str rest = str_from(list), part;
	while (str_next_field(&rest, ',', &part)) {
		strvec_push_take(out, str_dup(part));
	}
}
