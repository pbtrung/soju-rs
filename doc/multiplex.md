# soju.im/multiplex

A client of the bouncer usually opens one connection per bouncer network
(`soju.im/bouncer-networks`), plus one for the bouncer itself. With this
extension, it attaches the networks to its bouncer connection instead: a
single connection carries all of them. Behind a proxy which counts or
limits connections, or on a mobile device, that's one TLS session and one
keepalive instead of one per network.

This is a soju-rs extension: upstream soju doesn't support it.

## Capability

The server advertises the `soju.im/multiplex` capability. A client which
requests it must also support `soju.im/bouncer-networks`, and should request
`soju.im/bouncer-networks-notify` to learn about the networks.

Only a connection which isn't bound to a network (no `BOUNCER BIND`, no
network in the username) can attach networks.

## Attaching a network

Once registered, the client attaches a network by its ID:

    BOUNCER ATTACH <netid>

This creates a *virtual connection* for the network, which behaves like a
connection bound to it with `BOUNCER BIND`: the server sends it the
registration burst of such a connection (`RPL_WELCOME` to the end of the
MOTD, channel JOINs and their state, backlog), then the network's messages.
Every message of the virtual connection is tagged with the network:

    @soju.im/network=<netid> <message>

The tag is sent whether or not the client enabled `message-tags`. It's added
to the message's other tags, e.g. `@soju.im/network=3;time=... PRIVMSG ...`.

The client sends messages to the network with the same tag. Messages without
it go to the bouncer connection itself.

Attaching a network which is already attached replaces its virtual
connection: the server sends a new registration burst.

Errors are replied on the bouncer connection, untagged:

    FAIL BOUNCER INVALID_NETID ATTACH <netid> :Invalid network ID
    FAIL BOUNCER INVALID_CONNECTION ATTACH :Networks can only be attached to the bouncer connection

Without the capability, `ATTACH` and `DETACH` fail with
`FAIL BOUNCER UNKNOWN_COMMAND`, and the tag is ignored.

## State of a virtual connection

A virtual connection is registered from the start: there's no `CAP`,
`NICK`, `USER` or SASL exchange. It inherits the bouncer connection's
nickname, account, away status and enabled capabilities, except
`soju.im/multiplex` and `soju.im/bouncer-networks-notify`: like a bound
connection, it gets no network notifications and can't attach networks.

The server then adjusts its capabilities to the network with `cap-notify`
(`CAP NEW`/`CAP DEL`), as for a bound connection, before `RPL_WELCOME`. The
client may `CAP REQ` on the virtual connection.

Everything else is per virtual connection, as if each was a separate
connection: batch reference tags, labels (`labeled-response`), `draft/multiline`
batches, ISUPPORT, case mapping, nickname. In particular, the same batch
reference tag may be used by several networks at once: the client must route
a message by its network tag before handling its batch tag.

## Detaching a network

    BOUNCER DETACH <netid>

Closes the virtual connection, with no reply. Detaching a network which
isn't attached does nothing: the server may have closed it meanwhile.

## Closing

When the server closes a virtual connection (the network was deleted, a
`QUIT` on it, an error), it sends a tagged `ERROR` first:

    @soju.im/network=<netid> ERROR :Closing link

The bouncer connection stays open; the client may attach the network again.
Closing the bouncer connection closes all its virtual connections.

A message tagged with a network which isn't attached is answered with an
error on the bouncer connection, except `PING` and `PONG`, which are
dropped: they may be late messages for a network just detached.

    FAIL <command> NETWORK_NOT_ATTACHED <netid> :Network not attached

## Example

    C: CAP REQ :batch message-tags soju.im/bouncer-networks soju.im/bouncer-networks-notify soju.im/multiplex
    ...registration...
    S: BATCH +1 soju.im/bouncer-networks
    S: @batch=1 BOUNCER NETWORK 3 name=libera;state=connected
    S: BATCH -1
    C: BOUNCER ATTACH 3
    S: @soju.im/network=3 :soju.example.org 001 alice :Welcome to soju, alice
    ...
    C: @soju.im/network=3 PRIVMSG #soju :hello
    S: @soju.im/network=3;time=2026-10-24T12:00:00.000Z :bob!bob@example.org PRIVMSG #soju :hi
    C: BOUNCER DETACH 3
