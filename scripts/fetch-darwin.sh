#!/usr/bin/env bash
# Fetch the macOS arm64 binary CI built for this commit into dist/reses-aarch64-apple-darwin.
#
#   scripts/fetch-darwin.sh
#
# The AWS SDK's HTTPS client links Apple's Security framework on macOS, and there's no macOS SDK
# in the Linux build container, so I don't cross-build the Mac binary any more. The macOS job in
# .github/workflows/ci.yml already builds it natively, runs the goldens and the #15 replace test
# on it, and uploads it. So this script finds that job's run for the commit you're on and
# downloads its artifact with gh. It never builds anything, so nothing runs on the Mac's own
# toolchain.
#
# The commit has to be pushed and CI has to have passed on it. A dirty tree gets refused, since
# the binary would then be for a different tree than the one here.
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"
ARTIFACT="reses-aarch64-apple-darwin"

# The binary has to match the tree, so uncommitted changes to tracked files stop the fetch.
if ! git diff --quiet HEAD --; then
  echo "the tree has uncommitted changes, so CI's binary wouldn't match it. Commit and push first." >&2
  exit 1
fi
SHA="$(git rev-parse HEAD)"

# The newest successful ci.yml run on exactly this commit.
RUN="$(gh run list --workflow ci.yml --commit "$SHA" --status success --limit 1 \
  --json databaseId -q '.[0].databaseId // empty')"
if [ -z "$RUN" ]; then
  echo "no successful CI run for $SHA yet. Push it, wait for CI, then run this again:" >&2
  echo "  gh run list --workflow ci.yml --commit $SHA" >&2
  exit 1
fi

# Downloaded into the repo's gitignored tmp/, then renamed into place (#15). The artifact zip
# drops the executable bit, so I set it back before it's placed.
mkdir -p tmp dist
back="$(mktemp -d "$REPO_ROOT/tmp/darwin.XXXXXX")"
trap 'rm -rf "$back"' EXIT
gh run download "$RUN" --name "$ARTIFACT" --dir "$back"
chmod 755 "$back/reses"
scripts/place-binary.sh "$back/reses" "dist/$ARTIFACT"
echo "fetched CI run $RUN's macOS binary for ${SHA:0:12} into dist/$ARTIFACT"
