# Upstream TLS troubleshooting

An upstream `HandshakeFailure` can indicate incompatible TLS settings even
when the server is reachable and its certificate is valid. soju-rs uses rustls,
which supports modern TLS but deliberately excludes legacy RSA key exchange.
RSA certificates still work with ECDHE; the restriction concerns key exchange,
not the certificate's key type. See the
[rustls explanation](https://docs.rs/rustls/0.23.46/rustls/manual/_02_tls_vulnerabilities/index.html#rsa-pkcs1-encryption).

## P2P-NET investigation

On 2026-11-18, the published `v0.99.26` Linux x86_64 GNU binary reproduced
this error connecting to `ircs://irc.p2p-network.net:6697` with an isolated
temporary database:

```text
failed to register: failed to read message: tls: received fatal alert: HandshakeFailure
```

OpenSSL connected to the same hostname and verified the certificate, but
negotiated TLS 1.2 with `AES256-GCM-SHA384`. This cipher uses legacy RSA key
exchange, which rustls cannot negotiate. Restricting OpenSSL to ECDHE also
produced a handshake failure against that hostname during the investigation.

The hostname returned several addresses with different TLS configurations.
Individual IPv4 probes using the original hostname for SNI and certificate
verification produced these results:

| Server address | Verified OpenSSL result |
| --- | --- |
| `139.162.18.53`, `172.238.39.10` | Legacy RSA cipher succeeded; ECDHE failed |
| `139.162.106.227`, `139.162.154.78`, `159.65.204.61`, `167.99.180.211` | ECDHE succeeded with TLS 1.2 |
| `144.217.95.54` | TLS 1.3 succeeded |

This points to a TLS compatibility problem on some servers in the DNS pool,
rather than an unreachable network or an invalid certificate. These are dated
observations, not a permanent server list. IPv6 endpoints were not individually
tested, and successful soju IRC registration against the compatible endpoints
was not verified. Some subsequent direct-IP probes returned `wrong version
number`; a final hostname probe still verified successfully with the legacy
RSA cipher.

## Workaround when you do not control the IRC server

Select a server that supports modern TLS. Prefer an individual server hostname
provided by the network, with a certificate valid for that name. If no suitable
hostname is available, a local hosts override can keep the original TLS name
while selecting a compatible address.

On the machine running soju, edit the hosts file:

```sh
sudoedit /etc/hosts
```

Add one mapping, replacing any existing override for this hostname:

```text
144.217.95.54 irc.p2p-network.net
```

`144.217.95.54` negotiated TLS 1.3 with `TLS_AES_256_GCM_SHA384` in the dated
probe above; verify it again before use. To test this IP directly while keeping
the correct SNI and certificate verification name:

```sh
openssl s_client -connect 144.217.95.54:6697 \
  -servername irc.p2p-network.net -verify_hostname irc.p2p-network.net \
  -verify_return_error -brief -tls1_3 </dev/null
```

Expect `Protocol version: TLSv1.3` and `Verification: OK`. The TLS 1.2 ECDHE
addresses in the table are alternatives if this endpoint becomes unavailable;
they do not establish TLS 1.3 support.

Keep the soju network address as `ircs://irc.p2p-network.net:6697`. Using the
numeric IP as the network address instead would change the TLS verification
name and can cause a certificate mismatch.

Check resolution and TLS from the same environment as soju:

```sh
getent ahosts irc.p2p-network.net
openssl s_client -connect irc.p2p-network.net:6697 \
  -servername irc.p2p-network.net -verify_hostname irc.p2p-network.net \
  -verify_return_error -brief -cipher ECDHE </dev/null
```

Confirm resolution selects the intended address and OpenSSL reports
`Verification: OK` with an ECDHE cipher or TLS 1.3. The `-cipher` option
restricts TLS 1.2 and earlier; TLS 1.3 uses separate cipher settings.

Reconnect the upstream network, or restart soju. For the service from
[getting started](getting-started.md#run-as-a-service):

```sh
sudo systemctl restart soju
```

A service restart reconnects all users and networks. For containers, apply the
mapping through the container configuration so it is visible to the soju
process; editing only the host's hosts file may not affect the container.

The override affects other applications on that machine and bypasses the
network's DNS rotation and failover. Remove it when the network fixes its TLS
configuration, or replace it with a newly verified endpoint if the selected
server becomes unavailable. Keep certificate verification enabled: changing
certificate trust or pinning a certificate does not add support for legacy RSA
key exchange.

[Documentation index](README.md) · [Getting started](getting-started.md)
