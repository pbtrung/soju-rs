# Getting started

This guide sets up a private SQLite-backed bouncer, enables TLS and creates an
account. Use the [server manual](soju.1.scd) for the full configuration and
BouncerServ reference, and [compatibility](compatibility.md) for upstream differences.

## Install the executables

Extract a [release archive] for the target platform, verify it against its
`SHA256SUMS`, and install both `soju` and `sojuctl` on the server. Alternatively,
[build from source](../README.md#building). The examples below assume both
executables are in `/usr/local/bin` and a system account named `soju` exists.

## Configure storage and TLS

Create `/etc/soju/config` with a configuration such as:

```text
hostname bouncer.example.org
db sqlite3 /var/lib/soju/main.db
message-store db
listen ircs://
tls /etc/soju/cert.pem /etc/soju/key.pem
listen unix+admin://
```

Replace the hostname and certificate paths for your installation. `ircs://`
listens on the default TLS port, 6697. The service account must be able to read
the certificate and key. Reload with SIGHUP after certificate renewal.

For a TLS-terminating TCP reverse proxy, replace the TLS listener and `tls`
directive with a loopback plaintext listener:

```text
listen irc://localhost
```

This defaults to port 6667 and is intended to be reachable through the proxy.
WebSocket/HTTPS listeners and trusted forwarded-identity configuration are
separate options described in the manual. Only configure proxy trust for the
actual proxy peers.

The database contains upstream passwords, private keys and history. Its
directory should be owned by `soju` with mode 0700. The default admin socket is
`/run/soju/admin`; its directory must exist and be writable by the service.
`sojuctl` connects to that socket, so it needs filesystem access to it.

## Run as a service

A minimal systemd unit is:

```ini
[Unit]
Description=soju IRC bouncer
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/soju -config /etc/soju/config
ExecReload=/bin/kill -HUP $MAINPID
User=soju
RuntimeDirectory=soju
RuntimeDirectoryMode=0700
StateDirectory=soju
StateDirectoryMode=0700
WorkingDirectory=/var/lib/soju
UMask=0077
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

`RuntimeDirectory` and `StateDirectory` create private socket/storage directories.
Install the unit as `/etc/systemd/system/soju.service`, then run as root:

```sh
systemctl daemon-reload
systemctl enable --now soju
```

For a foreground run,
prepare the directories first and run `soju -config /etc/soju/config` as the
service account.

Once the server is running, create the initial administrator using an account
with access to the admin socket:

```sh
sojuctl -config /etc/soju/config user create -username alice -password '<password>' -admin
```

Choose the username and password. Client accounts are distinct from the
system account running the service.

## Connect a client

Connect to the bouncer's TLS hostname and port, using the bouncer account's
username and password. A client supporting `soju.im/bouncer-networks`, such as
the tested Gamja/Senpai forks, discovers networks without a username suffix.
Create or manage networks through its UI or `/msg BouncerServ help`.

For other clients, configure a separate connection to the bouncer for each
upstream network. The username `alice/irc.libera.chat` selects that network;
if it does not exist, the bouncer can create it from the server address. The
password remains the bouncer account password. BouncerServ provides advanced
network and upstream-authentication settings.

Give separate client installations stable names when they need independent
delivery cursors: for example `alice/irc.libera.chat@laptop` and
`alice/irc.libera.chat@workstation`. These names are part of the bouncer login,
not upstream IRC nicknames.

## Upgrade an existing installation

Back up before opening an existing SQLite database with a newer binary.
Supported older databases migrate on startup to schema 35. Upstream Go soju
and earlier soju-rs cannot open the upgraded database; rollback requires a
compatible backup. See [backup and upgrade details](message-text-index.md#upgrades-and-compatibility).

[Documentation index](README.md) · [Resource limits](resource-limits.md)

[release archive]: https://github.com/pbtrung/soju-rs/releases
