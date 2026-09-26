#!/usr/bin/env bash
# Decide whether a release run may go ahead, and print `version=X` for $GITHUB_OUTPUT.
#
#   scripts/release-version.sh EVENT SHA
#
# EVENT is the GitHub event name (push, pull_request) and SHA the commit being released.
# stdout carries only the output line; every message goes to stderr, so the workflow can append
# stdout straight to $GITHUB_OUTPUT. tests/release_version.rs runs this against real repos.
#
# On a push (a real release) it stops when:
#   - Cargo.toml has no version;
#   - the tag v<version> already exists on origin, so the version wasn't bumped;
#   - origin can't be asked at all (a network error must never read as "no tag");
#   - SHA isn't on main, so release was pushed from somewhere other than main;
#   - any ci.yml push run on SHA failed, or a job in one was skipped, or none passed at all.
# A pull_request dry run only warns about an existing tag, and skips the main and CI checks,
# since a PR's commit isn't on main yet and CI may still be running on it.
#
# The CI check asks GitHub's API through `gh`, with GH_TOKEN and GH_REPO from the environment.
# A run still in progress is waited for, up to RESES_CI_WAIT_SECS (default an hour), polling
# every RESES_CI_POLL_SECS (default 30); the tests set both to keep things quick.
set -euo pipefail
event="$1"
sha="$2"
cd "$(dirname "$0")/.."

# Stop with a GitHub error annotation on stderr.
die() {
  echo "::error::$*" >&2
  exit 1
}

# Refuse the release unless ci.yml passed on this exact commit. I only count push runs: a
# pull_request run builds a merge ref, which is a different commit. Every run has to finish,
# none may fail, a cancelled one proves nothing either way, and at least one has to pass with
# every one of its jobs passing. GitHub calls a run a success when some of its jobs were
# skipped, which is why each passing run's jobs are read one by one.
ci_passed() {
  local repo="${GH_REPO:?GH_REPO names the repository to ask}"
  local wait="${RESES_CI_WAIT_SECS:-3600}" poll="${RESES_CI_POLL_SECS:-30}"
  local deadline=$((SECONDS + wait)) runs said_waiting=""

  # Wait until every run on the commit has finished, or the time runs out.
  while :; do
    runs=$(gh api --paginate "repos/$repo/actions/workflows/ci.yml/runs?head_sha=$sha&event=push&per_page=100" \
      --jq '.workflow_runs[] | "\(.id) \(.status) \(.conclusion // "")"') ||
      die "couldn't ask GitHub for the CI runs on $sha, so I can't tell whether CI passed"
    if ! awk 'NF && $2 != "completed" { found = 1 } END { exit !found }' <<<"$runs"; then
      break
    fi
    if [ "$SECONDS" -ge "$deadline" ]; then
      die "CI is still running on $sha after ${wait}s. Rerun this job once it has finished."
    fi
    [ -n "$said_waiting" ] || echo "CI is still running on $sha, waiting for it" >&2
    said_waiting=1
    sleep "$poll"
  done

  # No run at all means the commit would go out untested.
  [ -n "$runs" ] || die "no CI run on $sha. Push it to main and let CI finish before releasing it."

  # Any finished run that isn't a pass or a cancellation stops the release.
  local bad
  bad=$(awk 'NF && $3 != "success" && $3 != "cancelled" { print "run " $1 " ended " $3 }' <<<"$runs")
  [ -z "$bad" ] || die "CI didn't pass on $sha: $(echo "$bad" | paste -sd, -)"

  # Cancelled runs only count as nothing, so something has to have passed.
  local passed
  passed=$(awk '$3 == "success" { print $1 }' <<<"$runs")
  [ -n "$passed" ] || die "every CI run on $sha was cancelled, so none of them passed"

  # Each passing run's jobs, one by one.
  local id jobs failed
  for id in $passed; do
    jobs=$(gh api --paginate "repos/$repo/actions/runs/$id/jobs?per_page=100" \
      --jq '.jobs[] | "\(.conclusion // "")\t\(.name)"') ||
      die "couldn't ask GitHub for the jobs of CI run $id"
    [ -n "$jobs" ] || die "CI run $id on $sha lists no jobs, so it proves nothing"
    failed=$(awk -F'\t' 'NF && $1 != "success" { print $2 " was " ($1 == "" ? "unfinished" : $1) }' <<<"$jobs")
    [ -z "$failed" ] || die "CI run $id on $sha didn't pass every job: $(echo "$failed" | paste -sd, -)"
  done
  echo "CI passed on $sha" >&2
}

version=$(grep -m1 -E '^version = "' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/' || true)
if [ -z "$version" ]; then
  echo "::error::no version found in Cargo.toml" >&2
  exit 1
fi

set +e
git ls-remote --exit-code --tags origin "refs/tags/v$version" >/dev/null 2>&1
rc=$?
set -e
case "$rc" in
  0)
    if [ "$event" = push ]; then
      echo "::error::tag v$version already exists. Bump the version in Cargo.toml (and Cargo.lock) on main, then push main to release again." >&2
      exit 1
    fi
    echo "::warning::tag v$version already exists, so a push to release would stop here. This dry run carries on." >&2
    ;;
  2) ;;
  *)
    echo "::error::couldn't ask origin for its tags (git ls-remote exit $rc), so I can't tell whether v$version exists" >&2
    exit 1
    ;;
esac

if [ "$event" = push ]; then
  if ! git fetch -q origin main 2>/dev/null; then
    echo "::error::couldn't fetch main from origin to check where this release comes from" >&2
    exit 1
  fi
  if ! git merge-base --is-ancestor "$sha" FETCH_HEAD; then
    echo "::error::release is at $sha, which isn't on main. Release only commits that are on main: git push origin main:release" >&2
    exit 1
  fi
  ci_passed
fi

echo "releasing v$version" >&2
echo "version=$version"
