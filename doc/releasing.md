# Building and releasing

Run commands from the repository root. Normal source builds are described in
[README](../README.md#building); [development](development.md#required-checks)
covers verification before a release.

## Build local archives

The release builder uses local toolchains to produce Linux x86_64 GNU and fully
static musl executables. Install Rust 1.100.0 or newer with both target standard
libraries, a native C compiler, `musl-gcc`, CMake, Make, Perl, `tar`, `readelf`
and `sha256sum`. Distro Rust is supported; rustup is not required.

On Arch Linux, the workflow installs `rust`, `rust-musl`, `musl`, `gcc`,
`cmake`, `make` and `perl`, plus Git. The checked-in
[builder](../scripts/build-release.sh) verifies tools and target libraries
before building.

```sh
./scripts/build-release.sh
```

The version defaults to `v<package-version>` from Cargo.toml. An explicit
version must match it. Output in `dist/` contains two `.tar.gz` archives and
`SHA256SUMS`. Archives include both executables, the license, README, sample
configuration and documentation. Packaging verifies that neither musl binary
has an ELF interpreter or dynamic NEEDED entries.

`DIST_DIR` selects the output directory. `CARGO_TARGET_DIR` and `MUSL_TARGET_DIR`
select build directories. To use a different musl compiler, set
`CC_x86_64_unknown_linux_musl`. The builder defaults the musl linker to `cc`
because Rust supplies its own CRT and libc; some musl wrappers introduce an
unwanted dynamic loader.

## Verify an archive

From the archive directory, run `sha256sum --check SHA256SUMS`, then extract
the archive for the target. Keep `soju` and `sojuctl` together. `soju -version`
should exit 0; `sojuctl -help` prints usage and exits 2. Archive checks do not
replace integration tests or a test deployment against a disposable backup.

## GitHub workflows

Both workflows are manually dispatched:

| Workflow | Behavior |
| --- | --- |
| [Tests](../.github/workflows/tests.yml) | GNU/musl formatting, check, Clippy, builds, tests and rustdoc; GNU IRCv3, transport and Gamja checks |
| [Build and Release](../.github/workflows/release.yml) | Build/verify/upload archives; optionally publish them |

Run **Tests** on the same commit before publishing. **Build and Release** does
not depend on that test run. Its default `publish=false` only uploads artifacts.
Enabling **Publish archives as a GitHub Release** publishes under the package's
`v<version>` tag. An existing tag must resolve to the selected commit; an
existing release is rejected. The workflow creates the tag if absent.

## Release helpers

These commands change the repository and remote release state. Run them only
when deliberately preparing or replacing a release, from a clean `main` checkout
with authenticated `gh` and Git:

- `scripts/release-new.sh <new-version>` bumps Cargo.toml/Cargo.lock, commits,
  pushes and dispatches publication. It also needs Python 3.11 or newer.
- `scripts/release-replace.sh` replaces the current version's tag/release and
  dispatches publication. Existing release assets are deleted.

Before mutation, the helpers check that remote `main` has an active
`release.yml` matching the local definition and publishing input. Push workflow
changes first. This catches missing, disabled or stale workflows; later network
or service errors can still prevent dispatch after mutations.

Validate helper behavior without publishing:

```sh
python3 tests/release_scripts.py
```

The regression suite stubs Git/GitHub commands. Do not execute a release helper
as a syntax or behavior test.

[Documentation index](README.md) · [Database upgrades](message-text-index.md#upgrades-and-compatibility)
