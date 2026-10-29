// IP addresses and networks with the parsing rules of the Rust code:
// std::net (IpAddr, SocketAddr) and the ipnet crate (IpNet), which differ:
// ipnet's IPv4 octets may have leading zeros ("010.0.0.1"), std's can't.
#pragma once

#include <stdint.h>

#include "base/err.h"
#include "base/str.h"

struct ipaddr {
	bool v6;
	uint8_t b[16]; // IPv4: the first 4 bytes
};

struct ipnet {
	struct ipaddr addr;
	uint8_t prefix;
};

// std's IpAddr::from_str
[[nodiscard]] bool ipaddr_parse(struct str s, struct ipaddr *out);
// std's SocketAddr::from_str: "1.2.3.4:80" or "[::1%3]:80"
[[nodiscard]] bool socketaddr_parse(struct str s, struct ipaddr *addr, uint16_t *port);
// IpAddr::to_canonical: an IPv4-mapped IPv6 address becomes IPv4
[[nodiscard]] struct ipaddr ipaddr_canonical(struct ipaddr a);
// Formats like Rust's Display for IpAddr (RFC 5952 for IPv6)
void ipaddr_format(struct strbuf *out, struct ipaddr a);

// The ipnet crate's IpNet::from_str ("10.0.0.1/8", "fd00::/64")
[[nodiscard]] bool ipnet_parse(struct str s, struct ipnet *out);
// IpNet::trunc: the host bits cleared
[[nodiscard]] struct ipnet ipnet_trunc(struct ipnet n);
// IpNet::contains for an address of the same family
[[nodiscard]] bool ipnet_contains(struct ipnet n, struct ipaddr a);
