#!/usr/bin/env bash
# Put a built binary at its destination path.
#
#   scripts/place-binary.sh SRC DST
set -euo pipefail
cp "$1" "$2"
