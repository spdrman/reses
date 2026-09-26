#!/usr/bin/env bash
# Regression test for #15, macOS only. It installs FIRST with scripts/place-binary.sh and runs
# it, then installs SECOND over it while another process holds the installed file open, and
# runs that. The holder stands in for the Docker Desktop VM, which keeps an open descriptor on
# every file it has served through a bind mount, dist/ included.
#
# On Apple Silicon the kernel keeps code-signing state on the inode of a binary that has run.
# Writing new contents into that inode while something else holds it open leaves the inode dead
# for exec: every later run is SIGKILLed (exit 137), whatever the new bytes are, identical bytes
# included, and a hard link to it dies the same way. Only a new inode escapes, which is what a
# rename gives. With nothing holding the file, an in-place overwrite is harmless, so a test
# without the holder cannot fail.
#
#   tests/macos-replace-binary.sh FIRST [SECOND]
#
# SECOND defaults to FIRST: that is `make darwin` twice on an unchanged tree, which is exactly
# how the bug first showed up. Identical bytes are killed too, so FIRST alone is a full test.
set -euo pipefail
[ "$(uname -s)" = Darwin ] || { echo "skipped: macOS only" >&2; exit 0; }
abs() { echo "$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"; }
repo="$(cd "$(dirname "$0")/.." && pwd)"
first="$(abs "$1")"
second="$(abs "${2:-$1}")"
mkdir -p "$repo/dist"
dir="$(mktemp -d "$repo/dist/.replace-test.XXXXXX")"
trap 'exec 7<&- 8<&-; rm -rf "$dir"' EXIT

run() { # run BIN; prints the exit status, 137 for a SIGKILL at exec
  set +e
  "$1" --version >/dev/null 2>&1
  local rc=$?
  set -e
  echo "$rc"
}

"$repo/scripts/place-binary.sh" "$first" "$dir/reses"
rc="$(run "$dir/reses")"
if [ "$rc" -ne 0 ]; then
  echo "FIRST does not run even before being replaced (exit $rc): $first" >&2
  exit 1
fi
exec 7<"$dir/reses"
"$repo/scripts/place-binary.sh" "$second" "$dir/reses"
rc="$(run "$dir/reses")"
if [ "$rc" -ne 0 ]; then
  echo "the binary placed over one that already ran, while it was held open, is killed (exit $rc)" >&2
  exit 1
fi
echo "a binary placed over one that already ran, while it was held open, still runs"

# Positive control, so a macOS that stops killing in-place overwrites is noticed rather than
# silently making this test unable to fail. Locally it is a notice. In CI it fails, because a
# step that cannot fail is worth knowing about there, and the message says the kernel changed,
# not reses. RESES_REPLACE_CONTROL=warn turns that failure into a warning: release.yml sets it,
# because a runner image with a new kernel says nothing about the binary being released.
cp "$first" "$dir/control"
run "$dir/control" >/dev/null
exec 8<"$dir/control"
cp "$second" "$dir/control"
rc="$(run "$dir/control")"
if [ "$rc" -eq 0 ]; then
  msg="a plain in-place cp under a holder now runs on this macOS: the kernel behaviour behind #15 has changed, so this test can no longer catch it (reses itself is fine)"
  if [ -n "${GITHUB_ACTIONS:-}" ] && [ "${RESES_REPLACE_CONTROL:-}" = warn ]; then
    echo "::warning::$msg" >&2
  elif [ -n "${GITHUB_ACTIONS:-}" ]; then
    echo "::error::$msg" >&2
    exit 1
  fi
  echo "note: $msg"
else
  echo "control: a plain in-place cp under a holder is still killed (exit $rc), so this test can catch #15"
fi
