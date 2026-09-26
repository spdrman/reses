#!/usr/bin/env bash
# Decode every mail fixture with a built reses binary and compare against the goldens that
# python/reses.py produced. Used by the macOS CI job and by `make darwin`, on the real binary.
#
#   scripts/check-goldens.sh path/to/reses
set -euo pipefail
# Resolve the binary before changing directory, so a relative path still works.
bin="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
cd "$(git rev-parse --show-toplevel)/tests/fixtures/mail"
n=0
for f in *.eml; do
  [ -e "$f" ] || break
  base=${f%.eml}
  "$bin" "$f" | diff -u "$base.out" -
  "$bin" --html "$f" | diff -u "$base.html.out" -
  n=$((n + 1))
done
# Finding nothing would pass without checking anything, so that is a failure too.
if [ "$n" -lt 10 ]; then
  echo "only $n mail fixtures found; expected at least 10" >&2
  exit 1
fi
echo "$n fixtures match their goldens (plain and --html)"
