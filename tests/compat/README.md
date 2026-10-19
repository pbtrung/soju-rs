# Compatibility tests

Tools used to check soju-rs against upstream Go soju, the IRCv3
specifications and the gamja web client. They start local servers on ports
1666x-1669x and 1679x.

    ./setup.sh                 # build Go soju and ergo, generate configs
    cargo build                # build soju-rs (debug binaries are used)
    ./run-diff.sh [scenario...]           # differential tests, report in work/difflatest.txt
    ./env-up.sh && python3 ircv3spec.py 16670 16667   # IRCv3 spec checks
    ./env-up.sh && python3 plumbing.py rs             # TLS, client certs, PROXY, identd, ...
    ./run-gamja.sh rs go       # gamja end-to-end tests (needs Node.js and Chromium)

`setup.sh` takes `SOJU_REF` (upstream soju git ref) and `GAMJA_SRC` (local
gamja checkout, defaults to cloning upstream). `EXTRA_CONFIG` adds lines to
both bouncers' configuration, `%D%` being replaced by each bouncer's
directory, e.g. `EXTRA_CONFIG="message-store fs %D%/logs" ./run-diff.sh`.

The differential harness (`diffharness.py`) runs each scenario against both
bouncers, each connected to its own ergo instance, and compares the
normalized lines received by every client. Volatile data (times, message
IDs, cloaks, ports) is normalized, and differences which only come from Go's
random map iteration order are accepted.
