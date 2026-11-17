#!/bin/bash
# Shared preflight for release helpers, called from the repository root.

RELEASE_WORKFLOW=release.yml

preflight_release_workflow() {
	local repository=$1 state remote_workflow
	if [[ ! -f ".github/workflows/$RELEASE_WORKFLOW" ]]; then
		printf 'Missing local release workflow: %s\n' "$RELEASE_WORKFLOW" >&2
		return 1
	fi
	state=$(gh api "repos/$repository/actions/workflows/$RELEASE_WORKFLOW" --jq .state) || return
	if [[ "$state" != active ]]; then
		printf 'Release workflow is not active: %s\n' "$state" >&2
		return 1
	fi
	remote_workflow=$(gh workflow view "$RELEASE_WORKFLOW" --ref main --repo "$repository" --yaml) || return
	# Dispatch uses main. Compare its complete definition with the reviewed
	# local workflow, including the workflow_dispatch/publish input contract.
	# Require workflow changes to be pushed before running a release helper.
	if [[ "$remote_workflow" != "$(<".github/workflows/$RELEASE_WORKFLOW")" ]]; then
		printf 'Release workflow on main differs from the local definition; reconcile it before releasing.\n' >&2
		return 1
	fi
}
