#!/bin/bash
# Publish a new stable version through GitHub Actions, without running Cargo.
# Usage: scripts/release-new.sh [v]MAJOR.MINOR.PATCH
# Requires a clean main checkout, Python 3.11+, Git and authenticated gh.
set -euo pipefail

if [[ $# != 1 || "$1" == --help || "$1" == -h ]]; then
	printf 'Usage: %s [v]MAJOR.MINOR.PATCH\n' "$0"
	if [[ $# == 1 && ( "$1" == --help || "$1" == -h ) ]]; then exit 0; fi
	exit 2
fi
version="${1#v}"
if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
	printf 'Expected a stable version such as 0.99.26.\n' >&2
	exit 2
fi
cd "$(dirname "$0")/.."
for tool in git gh python3; do
	if ! command -v "$tool" >/dev/null; then
		printf 'Missing required tool: %s\n' "$tool" >&2
		exit 1
	fi
done
if [[ "$(git branch --show-current)" != main || -n "$(git status --porcelain)" ]]; then
	printf 'Run from a clean main checkout, including no staged or untracked files.\n' >&2
	exit 1
fi
gh auth status >/dev/null
origin_url="$(git remote get-url origin)"
repository="$(gh repo view "$origin_url" --json nameWithOwner --jq .nameWithOwner)"
git fetch --no-tags origin main
git merge-base --is-ancestor FETCH_HEAD HEAD || {
	printf 'main is behind or has diverged from origin/main; reconcile it first.\n' >&2
	exit 1
}
tag="v$version"
if git ls-remote --exit-code --tags origin "refs/tags/$tag"; then
	printf 'Tag %s already exists; use release-replace.sh to replace it.\n' "$tag" >&2
	exit 1
else
	status=$?
	if [[ "$status" != 2 ]]; then exit "$status"; fi
fi

python3 - "$version" <<'PY'
import re
import sys
import tomllib
from pathlib import Path

version = sys.argv[1]
manifest_path, lock_path = Path("Cargo.toml"), Path("Cargo.lock")
manifest, lock = manifest_path.read_text(), lock_path.read_text()
package = tomllib.loads(manifest)["package"]
old = package["version"]
if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", old):
    raise SystemExit("The current package version must also be a stable MAJOR.MINOR.PATCH.")
if tuple(map(int, version.split("."))) <= tuple(map(int, old.split("."))):
    raise SystemExit(f"New version {version} must be greater than {old}.")

section = re.search(r"(?ms)^\[package\]\n.*?(?=^\[|\Z)", manifest)
if section is None:
    raise SystemExit("Cannot locate the package section in Cargo.toml.")
updated, count = re.subn(r'(?m)^version\s*=\s*"[^\"]+"', f'version = "{version}"', section[0])
if count != 1:
    raise SystemExit("Expected one package version in Cargo.toml.")
manifest = manifest[:section.start()] + updated + manifest[section.end():]

matches = []
for block in re.finditer(r"(?ms)^\[\[package\]\]\n.*?(?=^\[\[package\]\]|\Z)", lock):
    entry = tomllib.loads(block[0])["package"][0]
    if entry["name"] == package["name"] and "source" not in entry:
        matches.append((block, entry))
if len(matches) != 1 or matches[0][1]["version"] != old:
    raise SystemExit("Cargo.lock must contain exactly one matching local package version.")
block = matches[0][0]
updated, count = re.subn(r'(?m)^version\s*=\s*"[^\"]+"', f'version = "{version}"', block[0])
if count != 1:
    raise SystemExit("Expected one local package version in Cargo.lock.")
lock = lock[:block.start()] + updated + lock[block.end():]
# Validate both complete documents before changing either file.
tomllib.loads(manifest)
tomllib.loads(lock)
manifest_path.write_text(manifest)
lock_path.write_text(lock)
print(f"Updated {package['name']} from {old} to {version}.")
PY

git add -- Cargo.toml Cargo.lock
git commit -m "Release $tag" -m "Bump the package version and lockfile to $version for the GitHub release."
git push origin HEAD:refs/heads/main
gh workflow run ci.yml --ref main -F publish=true --repo "$repository"
printf 'Release workflow started for %s; it creates the tag after checks pass.\n' "$tag"
