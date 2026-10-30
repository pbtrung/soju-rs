# Phase 11 qualification

Status on 2026-10-30, at the commit adding this file. The version is
v0.99.0; the archives are built locally, no release has been published.
The behavioral oracle is soju-rs v0.99.24 (tools/oracle.sh).

Passed:

- Three-way differential harness (tests/compat/run-diff.sh): all 25
  scenarios match the Rust oracle. The bouncerserv scenario waits for
  BouncerServ's background work (password hashing, key generation, user
  deletion), whose timing made the oracle's own output vary.
- IRCv3 spec checks (tests/compat/ircv3spec.py): 28/32 for C, the same
  as Rust. The same four CAP checks fail on both: they encode
  expectations soju itself doesn't follow.
- Transport plumbing (tests/compat/plumbing.py c): 20/20, including TLS
  client certificates and CLIENTCERT.
- Compiler/sanitizer matrix (tools/ci.sh): GCC and Clang debug, ASan,
  TSan, release and UBSan-trap; Clang MSan and the fuzz build.
- Native Docker toolchains (tools/docker-test.sh): GNU gcc-valgrind,
  clang-valgrind, gcc-debug and gcc-release; musl gcc-debug, clang-debug
  and gcc-release. Valgrind isn't installed on the host, its presets only
  run in the GNU image. The two suppressions in tests/valgrind.supp link
  curl/curl#22363.
- clang-tidy on the files changed in phase 11.
- Ten-minute soak (tools/soak.sh, gcc-release) with two Ergo servers,
  200 users and 2,000 clients: 601 s, 556 reconnects, peak RSS 189,300
  KiB with a zero RSS slope, clean shutdown.
- Release archives (scripts/build-release.sh), each built and tested in
  its native image:
  - soju-v0.99.0-x86_64-linux-gnu.tar.gz
    `fac26c66e46365660a29e45f3061fd2b95dd9057de912df3c373e11c97d82c13`
  - soju-v0.99.0-x86_64-linux-musl.tar.gz
    `6b2695a13661eeb6d5afe90b5a0d06f3556aeb4816d5c95cb5280c34d7a53d51`

Not passed or not run:

- gamja end-to-end (tests/compat/run-gamja.sh): the first test fails the
  same way against C and Rust: the stock gamja checkout the suite uses
  (build/compat/gamja) doesn't expose the "Add network" dialog by the role
  the test looks for. It doesn't tell C and Rust apart; the test needs
  updating, or the suite pointing at the project's gamja.
- A controlled performance comparison with Rust: the soak figures above
  and the earlier Rust soak (601 s, peak RSS 591,312 KiB) are separate
  runs, not a controlled throughput comparison.

The soak budget is 10 minutes, replacing the old 24-hour release soak.
Longer fuzz budgets and heap attribution were not run.
