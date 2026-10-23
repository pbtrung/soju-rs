# Getting started

## Server side

Download a release archive for your platform from the [releases page], or build
from source with `cargo build --release` (see the [README]).

soju's configuration file (for instance `/etc/soju/config`, passed with
`-config`) needs to be adjusted to enable TLS. This can be done in several
ways:

- By setting up a reverse proxy which takes care of terminating TLS
  connections, and configuring soju to listen on local unencrypted connections
  on port 6667:

      listen irc://localhost

- By specifying the path to the TLS certificate in the soju configuration file:

      tls <cert> <key>

  The certificate must be readable by soju, and soju needs to be reloaded
  (SIGHUP) when the certificate is renewed.

Users are managed through the admin socket with `sojuctl`. Enable it in the
configuration file:

    listen unix+admin://

The default socket path is `/run/soju/admin`: its directory must exist and be
writable by soju (with systemd, `RuntimeDirectory=soju` creates it).

The database (`db sqlite3 <path>`) holds upstream passwords, private keys and
chat history: keep it in a directory only soju can access, for instance
`/var/lib/soju` owned by the soju user with mode 0700 (with systemd,
`StateDirectory=soju` and `UMask=0077`).

A minimal systemd unit:

    [Unit]
    Description=soju IRC bouncer
    After=network-online.target
    Wants=network-online.target

    [Service]
    ExecStart=/usr/local/bin/soju -config /etc/soju/config
    ExecReload=/bin/kill -HUP $MAINPID
    User=soju
    RuntimeDirectory=soju
    StateDirectory=soju
    WorkingDirectory=/var/lib/soju
    UMask=0077
    Restart=on-failure

    [Install]
    WantedBy=multi-user.target

Start soju (e.g. via systemd or another supervisor daemon), then create an
initial user:

    sojuctl -config /etc/soju/config user create -username <soju username> -password <password> -admin

soju-rs uses the same database as upstream soju: an existing `soju.db` can be
reused as is.

## Client side

### Client supporting `soju.im/bouncer-networks`

If you are using a client supporting the `soju.im/bouncer-networks` IRC
extension (for instance [gamja]), then you can just connect to soju with your
username and password.

If your client doesn't provide a UI to manage IRC networks, you can talk to
`BouncerServ`. See the [man page](soju.1.scd) or use `/msg BouncerServ help`.

### Other clients

You will need to setup one separate server in your client for each server you
want soju to connect to.

The easiest way to get started is to specify the IRC server address directly in
the username in the client configuration. For example to connect to Libera Chat,
your username will be: `<soju username>/irc.libera.chat`. Also set your soju
password in the password field of your client configuration.

This will autoconfigure soju by adding a network with the address
`irc.libera.chat` and then autoconnect to it. You will now be able to join
any channel like you would normally do.

For more advanced configuration options, you can talk to `BouncerServ`. See the
[man page](soju.1.scd) or use `/msg BouncerServ help`.

If you intend to connect to the bouncer from multiple clients, you will need to
append a client name in your username. For instance, to connect from a laptop
and a workstation, you can setup each client to use the respective usernames
`<soju username>/irc.libera.chat@laptop` and
`<soju username>/irc.libera.chat@workstation`.

[releases page]: https://github.com/pbtrung/soju-rs/releases
[README]: ../README.md
[gamja]: https://codeberg.org/emersion/gamja
