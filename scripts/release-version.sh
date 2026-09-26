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
#   - SHA isn't on main, so release was pushed from somewhere other than main.
# A pull_request dry run only warns about an existing tag, and skips the main check, since a
# PR's commit isn't on main yet.
set -euo pipefail
event="$1"
sha="$2"
cd "$(dirname "$0")/.."

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
fi

echo "releasing v$version" >&2
echo "version=$version"
