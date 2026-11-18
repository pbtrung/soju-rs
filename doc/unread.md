# soju.im/unread

When a client opens, it wants to show which conversations have unread
messages, and which of them mention the user, across every network. Loading
each conversation's missed messages to find out costs the bouncer a history
query per busy channel at every connect, most of them for conversations the
user won't open. With this extension, the bouncer tells the client directly,
and the client loads a conversation's history only once it is opened.

This is a soju-rs extension: upstream soju doesn't support it.

## Capability

The server advertises the `soju.im/unread` capability. A client requesting
it should also support `draft/chathistory` and a read marker extension
(`draft/read-marker`), since unread state is relative to the read markers.

## Tags

With the capability enabled, each `CHATHISTORY TARGETS` reply line may carry
two tags without values:

    @soju.im/unread;soju.im/mention CHATHISTORY TARGETS <target> <timestamp>

- `soju.im/unread`: the target has a `PRIVMSG` or `NOTICE` after its read
  marker, or has no read marker. A client may still not show a target
  holding only private notices, as with any notice. Events (joins, parts…)
  don't count, with or without `draft/event-playback`.
- `soju.im/mention`: a message after the read marker mentions the user. In a
  channel, that's a message containing the user's nick; in a private
  conversation, any `PRIVMSG` from the other user (not `NOTICE`s, e.g. from
  services, nor CTCP requests and replies other than `ACTION`), since a
  client shows notices elsewhere. The user's own messages and
  messages from blocked users (the `blocked` target metadata, see
  `draft/metadata-2`) don't count. Muted
  targets are tagged too: whether a mention in one should notify is up to the
  client.

The tags are sent whether or not the client enabled `message-tags`, like
`time` with `server-time`.

A client wanting historical state lists targets over a time range covering
the conversations it needs, for instance:

    CHATHISTORY TARGETS timestamp=<now> timestamp=1970-01-01T00:00:00.000Z 1000

`<now>` is a placeholder for a server-time timestamp. If an upstream server's
clock is ahead, use an upper bound covering its latest messages. The limit caps
the number of returned targets; this example is not pagination over an
unbounded target list.

The state is a snapshot. Afterwards, the client follows new messages and
`MARKREAD` updates as usual: a read marker moving past a target's latest
message reads it.

## Server

The server keeps, for each conversation, the time of the latest message
mentioning the user, in the `TargetHighlight` table (schema 35), keyed by
network and target name. Mention updates are coalesced for one second before
being written; actor/database delays or retries can extend that interval.
Target listings reflect persisted state, not pending updates. Reading a target
updates its read marker rather than its mention timestamp.

Comparing text and mention times with `ReadReceipt` gives the two tags.
Text-only target listings use the message text index for the latest text
time. With event playback enabled, the listing also seeks the clustered
message table for each target's latest event. Neither path loads the full
conversation history. See [message storage](message-text-index.md).

The user's own messages after the read marker make a target unread: clients
which send messages should move the read marker, as gamja does when the
target is on screen.

[Documentation index](README.md) · [Multiplexing](multiplex.md)
