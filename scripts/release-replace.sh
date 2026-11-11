#!/bin/bash
# Replace the current version's release and tag, then rebuild through Actions.
# Usage: scripts/release-replace.sh
# Requires a clean main checkout, Git and authenticated gh. No Cargo is run.
# This deletes existing release assets and moves only the current version's tag.
set -euo pipefail

if [[ $# != 0 ]]; then
	printf 'Usage: %s\n' "$0"
	if [[ $# == 1 && ( "$1" == --help || "$1" == -h ) ]]; then exit 0; fi
	exit 2
fi
cd "$(dirname "$0")/.."
for tool in git gh; do
	if ! command -v "$tool" >/dev/null; then
		printf 'Missing required tool: %s\n' "$tool" >&2
		exit 1
	fi
done
if [[ "$(git branch --show-current)" != main || -n "$(git status --porcelain)" ]]; then
	printf 'Run from a clean main checkout, including no staged or untracked files.\n' >&2
	exit 1
fi
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)"
if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
	printf 'Expected a stable package version in Cargo.toml.\n' >&2
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
ref="refs/tags/$tag"
# Record the unpeeled tag object, including for annotated tags. The lease
# refuses to overwrite a tag changed by someone else after this lookup.
if remote_tag="$(git ls-remote --exit-code --tags origin "$ref")"; then
	read -r old_oid _ <<< "$remote_tag"
else
	status=$?
	if [[ "$status" != 2 ]]; then exit "$status"; fi
	old_oid=""
fi
# Listing with pagination distinguishes an absent release from API failures.
release_id="$(gh api --paginate "repos/$repository/releases?per_page=100" \
	--jq ".[] | select(.tag_name == \"$tag\") | .id")"
# Push the branch before deleting any release. A rejected push stops here.
git push origin HEAD:refs/heads/main
git push "--force-with-lease=$ref:$old_oid" origin "HEAD:$ref"
git update-ref "$ref" HEAD
if [[ -n "$release_id" ]]; then
	gh release delete "$tag" --yes --repo "$repository"
fi
gh workflow run ci.yml --ref main -F publish=true --repo "$repository"
printf 'Replacement workflow started for %s at %s.\n' "$tag" "$(git rev-parse HEAD)"
