#!/usr/bin/env bash
# Put a built binary at its destination path, on a new inode.
#
#   scripts/place-binary.sh SRC DST
#
# Never overwrite DST in place. On Apple Silicon the kernel keeps code-signing state on the
# inode of a binary that has run, and if new contents are written into that inode while
# another process holds it open, every later exec of it is SIGKILLed, identical bytes included.
# Anything can be that holder: the Docker Desktop VM holds every file it has seen under a bind
# mount, dist/ among them, and a reses that is still running holds its own binary. So a plain
# cp over the previous build hits exactly that (#15). Copying beside the destination and
# renaming over it gives DST a fresh inode each time, and the rename is atomic, so there is no
# moment with no binary at the path. Runs on the host and inside the CI container alike.
#
# The temp name comes from mktemp, not $$, because pids repeat between container runs and two
# builds at once would otherwise share one temp file. A run killed with SIGKILL can't clean up,
# so a stray DST.tmp.XXXXXX can be left behind; it's never executed and is safe to delete.
set -euo pipefail
src="$1"
dst="$2"
if [ -d "$dst" ]; then
  echo "place-binary: $dst is a directory; give the full destination file path" >&2
  exit 1
fi
tmp="$(mktemp "$dst.tmp.XXXXXX")"
trap 'rm -f "$tmp"' EXIT
cp "$src" "$tmp"
# mktemp makes the file 0600 and cp keeps that, so carry the source's mode across.
# GNU stat takes -c; BSD stat rejects -c, and GNU would misread BSD's -f as "filesystem".
chmod "$(stat -c %a "$src" 2>/dev/null || stat -f %Lp "$src")" "$tmp"
mv -f "$tmp" "$dst"
