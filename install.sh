#!/bin/sh
# Install re:SES on Debian, Ubuntu or a derivative, from the latest GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/spdrman/reses/main/install.sh | sh
#
# I find the latest release by following GitHub's releases/latest redirect, download the .deb
# for this machine's architecture along with the release's SHA256SUMS, check the package against
# its listed checksum, and only then hand it to apt, which installs it like any other package
# (and removes it with `apt remove reses`). Set RESES_VERSION=v1.2.3 to install a particular
# release instead of the latest. On a Mac, Homebrew is the way: brew install spdrman/reses/reses
set -eu

repo=spdrman/reses

say() { printf 'reses installer: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }

# This installer only makes sense where apt is the package manager.
[ "$(uname -s)" = Linux ] || die "this installer is for Debian and Ubuntu. On a Mac, use: brew install spdrman/reses/reses"
command -v dpkg >/dev/null 2>&1 && command -v apt-get >/dev/null 2>&1 \
  || die "apt and dpkg are needed (Debian, Ubuntu or a derivative). Other systems: https://github.com/$repo/releases"
for tool in curl sha256sum; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool is needed"
done

# Releases ship packages for amd64 and arm64 only.
arch=$(dpkg --print-architecture)
case "$arch" in
  amd64 | arm64) ;;
  *) die "there's no reses package for $arch, only amd64 and arm64" ;;
esac

# The release tag, from RESES_VERSION or from where releases/latest redirects to.
tag=${RESES_VERSION:-}
if [ -z "$tag" ]; then
  latest=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest") \
    || die "couldn't reach GitHub to find the latest release"
  tag=${latest##*/}
fi
case "$tag" in
  v[0-9]*) ;;
  *) die "couldn't work out which release to install (got '$tag')" ;;
esac
version=${tag#v}
deb="reses_${version}_${arch}.deb"
base="https://github.com/$repo/releases/download/$tag"

# Download into a private temp directory, readable by apt's sandbox user, and clean it up.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
chmod 0755 "$tmp"
say "downloading $deb from $tag"
curl -fsSL -o "$tmp/$deb" "$base/$deb" || die "couldn't download $base/$deb"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || die "couldn't download $base/SHA256SUMS"
chmod 0644 "$tmp/$deb"

# The package must be listed in SHA256SUMS and match it, or nothing gets installed.
grep "  $deb\$" "$tmp/SHA256SUMS" > "$tmp/want" || die "$deb isn't in the release's checksums, so I'm not installing it"
(cd "$tmp" && sha256sum -c want >/dev/null 2>&1) || die "checksum mismatch for $deb, so I'm not installing it"

# apt needs root; use sudo when this isn't already running as root.
sudo=""
if [ "$(id -u)" -ne 0 ]; then
  command -v sudo >/dev/null 2>&1 || die "run this as root, or install sudo"
  sudo=sudo
fi
say "installing with apt"
$sudo apt-get install -y "$tmp/$deb"

say "installed $(reses --version 2>/dev/null || echo "reses $version"). Run reses to open the inbox."
