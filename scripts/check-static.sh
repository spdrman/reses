#!/usr/bin/env bash
# Check that a Linux build of reses is fully static, so it runs on any distro.
#
#   scripts/check-static.sh path/to/reses
#
# Static means no program interpreter and no NEEDED libraries in the ELF headers, which is what
# readelf reads. x86_64 musl comes out static-pie, which file(1) words differently from
# aarch64's "statically linked", so the file(1) wording is only a positive control on top: if
# it ever calls the binary neither, something about the build changed and I want to know.
# release.yml, ci.yml's musl job and the local gate (scripts/ci-docker.sh) all run this, so the
# three can't drift into checking different things.
set -euo pipefail
bin="${1:?usage: check-static.sh path/to/reses}"

# A missing tool must fail the check, not quietly skip it.
for tool in readelf file; do
  command -v "$tool" >/dev/null || { echo "::error::$tool is missing, so staticness can't be checked" >&2; exit 1; }
done

# The positive control: what file(1) makes of it.
file "$bin"
file "$bin" | grep -Eq 'statically linked|static-pie linked' || { echo "::error::file(1) doesn't call $bin static" >&2; exit 1; }

# The real check: a dynamic binary names an interpreter, and names the libraries it needs.
if readelf -lW "$bin" | grep -q 'program interpreter'; then
  echo "::error::$bin has a program interpreter, so it is dynamically linked" >&2
  exit 1
fi
if readelf -dW "$bin" | grep -q NEEDED; then
  echo "::error::$bin needs shared libraries" >&2
  exit 1
fi
echo "$bin is static"
