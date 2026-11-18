# Compatibility tests

These tools start disposable local bouncers and Ergo servers, then exercise
protocol behavior, transports, normalized transcripts and a Gamja browser.
Run the commands below from the **repository root**. Use the
[development guide](../../doc/development.md) for the Rust suite and
[compatibility guide](../../doc/compatibility.md) for intentional upstream differences.

## Prepare the environment

Setup needs Git, Go, OpenSSL, Python, Node.js and npm. Browser checks also need
Chromium and its runtime dependencies. It builds Go soju and Ergo, creates test
certificates/configuration and installs/builds Gamja:

```sh
cargo build --locked
./tests/compat/setup.sh
```

By default, files are under `tests/compat/work/`. `COMPAT_WORK` selects another
absolute directory; export it consistently for setup and subsequent commands.
Only use a disposable test directory: startup recreates bouncer/server state
and accounts `alice`/`bob`, not an existing user's installation.

`SOJU_REF` selects the upstream soju revision. A fresh setup fetches Ergo and
the latest default-branch [pbtrung Gamja](https://github.com/pbtrung/gamja).
`GAMJA_SRC` instead copies tracked files from a local Gamja checkout and must
be an **absolute path**. Existing source/dependency directories can be reused;
setup is not a clean rebuild of every input. Record resolved Git revisions,
lockfiles and tool/browser versions when comparing results. The
[final assessment](../../doc/audit/final-verification.md#inputs-and-local-gates)
provides the inputs used for the recorded audit; setup defaults do not pin them.

## Run the suites sequentially

Environment-resetting commands stop the processes recorded in the test
workspace and replace their databases. Do not run these suites concurrently.
Other processes occupying ports 1666x–1669x or 1679x also cause failures.
Local TCP and Unix sockets must be permitted.

```sh
./tests/compat/env-up.sh
python3 tests/compat/ircv3spec.py 16670 16667
python3 tests/compat/plumbing.py rs
./tests/compat/run-diff.sh registration_bouncer
```

Pass selected scenario names, or omit arguments to run every registered scenario. The differential runner resets the environment for each
scenario and writes its report to `$COMPAT_WORK/difflatest.txt` (the default
workspace applies when the variable is unset). It compares both bouncers on
separate Ergo instances, normalizing times, IDs, cloaks, ports and supported
unordered output. Deferred service operations use reply completion barriers;
a PONG alone does not prove that a user operation has finished.

Differences from Rust-specific capabilities, service output or behavior remain
failures of transcript equivalence even if both clients work. Do not interpret
a failed comparison as proof of a client bug, or normalize away a meaningful
protocol difference to obtain a pass.

## Test a local Gamja fork

Before first setup, export `GAMJA_SRC=/absolute/path/to/gamja`. Setup copies
tracked files, not untracked changes or `.git` metadata. If the workspace
already contains Gamja dependencies, use a fresh `COMPAT_WORK` to test a new
source revision.

Use a system Chromium:

```sh
CHROMIUM_EXECUTABLE=/usr/bin/chromium ./tests/compat/run-gamja.sh rs go
```

Alternatively install Playwright's Chromium from the workspace's Gamja
directory with `npx playwright install --with-deps chromium`, then omit
`CHROMIUM_EXECUTABLE`. The runner starts a fresh environment and tests Rust
and/or Go in the requested order; reports and failure artifacts stay in the
selected workspace. CI uses Arch's system Chromium.

The four scenarios cover network management, bidirectional channel chat,
history/read markers/search, metadata/detach and BouncerServ. They do not cover
every client feature or a production reverse-proxy deployment.

## Test a local Senpai fork

Senpai's real-bouncer test lives in the Senpai checkout. Build or extract a
release with `soju` and `sojuctl` in the same directory, then run there:

```sh
SOJU_RS_BINARY=/absolute/path/to/soju go test ./... -count=1
```

The tested fork skips its real-bouncer integration when `SOJU_RS_BINARY` is
unset. With it supplied, the test uses disposable SQLite storage and two
simulated upstream networks. This is separate from the browser harness.

## Environment options and cleanup

- `RSBIN`: absolute Rust binary directory used by `env-up.sh` (default:
  repository `target/debug`). `plumbing.py` uses the separate variable `RS_BIN`;
  set both when using a target-specific or extracted build.
- `COMPAT_SOCK_DIR`: plumbing socket directory, default `/tmp`. Keep it separate
  from the workspace's `env/plumb-rs` configuration directory.
- `EXTRA_CONFIG`: directives added to both bouncers; `%D%` expands to each
  bouncer's directory, for example `motd %D%/motd.txt`.

`env-up.sh` stops prior processes recorded in its workspace after verifying
their working directories. After a run, stop only the test processes identified
by those PID files and directories; the scripts leave the environment running
for inspection. Keep any artifacts you need with your test report rather than
linking session-local paths from maintained documentation.
