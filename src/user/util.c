#include "user/user.h"

#include <openssl/evp.h>
#include <stdlib.h>

#include "base/alloc.h"

char *user_ident(int64_t id) {
	unsigned char input[64] = {0}, digest[EVP_MAX_MD_SIZE];
	uint64_t value = (uint64_t)id;
	for (unsigned i = 0; i < 8; i++) {
		input[i] = (unsigned char)(value >> (i * 8));
	}
	unsigned n;
	if (EVP_Digest(input, sizeof(input), digest, &n, EVP_sha256(), nullptr) != 1 || n != 32) {
		abort();
	}
	static constexpr char hex[] = "0123456789abcdef";
	AUTO_STRBUF struct strbuf out = STRBUF_INIT;
	for (unsigned i = 0; i < 16; i++) {
		strbuf_append_char(&out, hex[digest[i] >> 4U]);
		strbuf_append_char(&out, hex[digest[i] & 15U]);
	}
	return strbuf_take(&out);
}

bool user_local_tcp_addr(const struct ipnet *nets, size_t n, int64_t user_id, struct ipaddr remote,
			 struct ipaddr *out, bool *found, struct err **err) {
	*found = false;
	remote = ipaddr_canonical(remote);
	for (size_t i = 0; i < n; i++) {
		if (nets[i].addr.v6 != remote.v6) {
			continue;
		}
		struct ipaddr ip = ipnet_trunc(nets[i]).addr;
		uint64_t carry = (uint64_t)user_id + 1;
		size_t bytes = ip.v6 ? 16 : 4;
		for (size_t j = bytes; j > 0; j--) {
			uint64_t sum = ip.b[j - 1] + (carry & 255U);
			ip.b[j - 1] = (uint8_t)sum;
			carry = (carry >> 8U) + (sum >> 8U);
		}
		if (carry != 0 || !ipnet_contains(nets[i], ip)) {
			return err_set(err, 0, "IP network too small");
		}
		*out = ip;
		*found = true;
		return true;
	}
	return true;
}

bool user_network_highlight(const struct user *u, const struct user_network *net,
			    const struct irc_msg *msg) {
	if (!cstr_eq(msg->command, "PRIVMSG") && !cstr_eq(msg->command, "NOTICE")) {
		return false;
	}
	const struct upstream *uc = net->upstream;
	const char *nick =
		uc != nullptr && uc->attached ? uc->nick : db_get_nick(&u->record, &net->record);
	if (nick[0] == '\0') {
		return false;
	}
	enum casemapping cm = uc != nullptr ? uc->casemap : CASEMAPPING_RFC1459;
	AUTOFREE char *source = casemap_cstr(cm, irc_msg_prefix_name(msg));
	AUTOFREE char *self = casemap_cstr(cm, nick);
	return !cstr_eq(source, self) &&
	       is_highlight(str_from(irc_msg_param(msg, 1)), str_from(nick));
}
