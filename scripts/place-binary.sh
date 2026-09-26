#!/usr/bin/env bash
# Put a built binary at its destination path, on a new inode.
#
#   scripts/place-binary.sh SRC DST
#
# Never overwrite DST in place. On Apple Silicon the kernel keeps code-signing state on the
# inode of a binary that has run, and if new contents are written into that inode while
# another process holds it open, every later exec of it is SIGKILLed, identical bytes included.
# The Docker Desktop VM holds every file it has seen under a bind mount, dist/ among them, so a
# plain cp over the previous build hits exactly that (#15). Copying beside the destination and
# renaming over it gives DST a fresh inode each time, and the rename is atomic, so there is no
# moment with no binary at the path. Runs on the host and inside the CI container alike.
set -euo pipefail
src="$1"
dst="$2"
tmp="$dst.tmp.$$"
trap 'rm -f "$tmp"' EXIT
cp "$src" "$tmp"
mv -f "$tmp" "$dst"
