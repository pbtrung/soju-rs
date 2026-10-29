# soju-c

A C port of [soju-rs] (the `main` branch of this repository), itself a
Rust port of [soju], the user-friendly IRC bouncer. soju connects to
upstream IRC servers on behalf of its users and adds features on top:
multiple users and networks, detached channels, chat history playback and
many [IRCv3] extensions.

> **Status: work in progress.** The core is ported (phases 0–7 of
> [PLAN.md](PLAN.md)), but the `soju` and `sojuctl` programs do not run a
> server yet. They are wired up in phase 10. Until then `soju` only
> supports `-version` and `-build-info`.

## Features

soju-c aims to behave exactly like soju-rs. It uses the same configuration
file, reads and writes the same `soju.db` and speaks the same IRC protocol.

- Multiple users, each with multiple upstream networks
- Chat history in SQLite (the `db` message store) or in memory, served
  through `CHATHISTORY`, read markers and backlog replay
- Detached channels with auto-detach
- BouncerServ commands (`network`, `channel`, `certfp`, `sasl`, `user`,
  `server`, `help`)
- SASL PLAIN, EXTERNAL and OAUTHBEARER for clients; SASL and pinned
  certificate fingerprints for upstream servers
- IRCv3: CAP 302, labeled-response, batch, echo-message, server-time,
  chathistory, read-marker, metadata-2, bouncer-networks, MONITOR,
  multiline, and more
- `soju.im/multiplex`, which lets a client attach all of its networks to a
  single bouncer connection (used by [gamja])
- Listeners: `ircs`, `irc+insecure`, `unix`, `unix+admin`, PROXY protocol
  and identd. HTTP/WebSocket, Prometheus, Web Push, OAuth2/HTTP auth and
  Turso Cloud storage are in later phases
- Built with memory safety in mind. Every change is tested under ASan, UBSan,
  MSan, TSan and valgrind, and fuzzed

The soju-rs README on `main` lists the ways this differs from upstream soju.

## Installing

Requirements:

- Linux (glibc or musl)
- GCC or Clang with C23 support
- CMake 3.29 or newer and Ninja
- OpenSSL 3, libcurl and libxcrypt (development packages)
- Network access during the first configure

Other dependencies (libuv, SQLite, mimalloc, utf8proc,
yyjson, cmocka) are downloaded at the versions pinned in `deps/VERSIONS`.
Their checksums are verified, and they are built once per compiler and mode
and linked statically.

Build a release binary:

    cmake --preset gcc-release
    cmake --build --preset gcc-release
    sudo cmake --install build/gcc-release

This installs `soju` and `sojuctl` to `/usr/local/bin`. You can change the
location with `--prefix`. Use the `clang-*` presets to build with Clang.

Docker images for both targets are in `docker/`.
`tools/docker-test.sh <gnu|musl> [mode...]` builds and tests inside a
container.

## Usage

Once the server is ported, it is used the same way as soju-rs:

    soju -config /etc/soju/config
    sojuctl -config /etc/soju/config user create -username alice -password secret -admin

Then connect an IRC client to the bouncer, logging in as
`alice/<network>`, and manage networks with `/msg BouncerServ help`. The
configuration file and commands are documented in soju's man pages.

These options already work:

    soju -version       # print the version
    soju -build-info    # print the compiler, mode and library versions

## Development

Each build mode has a CMake preset (`gcc-debug`, `clang-asan`,
`clang-msan`, `gcc-tsan`, `gcc-valgrind`, `clang-fuzz`, ...):

    cmake --preset gcc-asan && cmake --build --preset gcc-asan
    ctest --preset gcc-asan

`tools/ci.sh [preset...]` runs every preset, and `tools/lint.sh` runs the
lint checks. See [PLAN.md](PLAN.md) for the design and testing strategy and
[doc/conventions.md](doc/conventions.md) for coding rules.

## License

AGPLv3, see [LICENSE](LICENSE).

Copyright (C) 2020 The soju Contributors

[soju]: https://soju.im
[soju-rs]: https://github.com/pbtrung/soju-rs
[IRCv3]: https://ircv3.net/
[gamja]: https://codeberg.org/emersion/gamja
