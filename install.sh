#!/bin/sh
# Install re:SES on Debian, Ubuntu or a derivative, from the latest GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/spdrman/reses/main/install.sh | sh
#
# I find the latest release by following GitHub's releases/latest redirect, download the .deb
# for this machine's architecture along with the release's SHA256SUMS, check the package against
# its listed checksum, and only then hand it to apt, which installs it like any other package.
# `apt remove reses` removes it. There's no apt repository, so `apt upgrade` won't update reses;
# run this again to upgrade. Set RESES_VERSION=0.2.1 (or v0.2.1) to install a particular release.
# On a Mac, use Homebrew instead: brew install spdrman/reses/reses
#
# The whole script is one function, called on the last line, so a download cut off partway
# through runs nothing at all.
set -eu

# Every step of the install, in order. Each check stops the script with a message, and nothing
# reaches apt until every check before it has passed.
main() {
  repo=spdrman/reses
  releases="https://github.com/$repo/releases"

  # This installer only makes sense where apt is the package manager.
  [ "$(uname -s)" = Linux ] || die "this installer is for Debian and Ubuntu. On a Mac, use: brew install spdrman/reses/reses"
  command -v dpkg >/dev/null 2>&1 && command -v apt-get >/dev/null 2>&1 \
    || die "apt and dpkg are needed (Debian, Ubuntu or a derivative). For other systems, see $releases"
  for tool in curl sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is needed"
  done

  # Releases ship packages for amd64 and arm64 only.
  arch=$(dpkg --print-architecture)
  case "$arch" in
    amd64 | arm64) ;;
    *) die "there's no reses package for $arch, only for amd64 and arm64" ;;
  esac

  # The release tag, from RESES_VERSION or from where releases/latest redirects to. Only a
  # plain version is accepted, so nothing in it can steer the download anywhere else.
  tag=${RESES_VERSION:-}
  if [ -z "$tag" ]; then
    latest=$(curl --proto '=https' -fsSLI -o /dev/null -w '%{url_effective}' "$releases/latest") \
      || die "couldn't reach GitHub to find the latest release"
    tag=${latest##*/}
  fi
  case "$tag" in v*) ;; *) tag="v$tag" ;; esac
  printf '%s\n' "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$' \
    || die "couldn't work out which release to install (got '$tag'); use a version like 0.2.1"
  version=${tag#v}
  deb="reses_${version}_${arch}.deb"
  base="$releases/download/$tag"

  # Download into a private temp directory that apt's sandbox user can read, and clean it up.
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  chmod 0755 "$tmp"
  say "downloading $deb from $tag"
  curl --proto '=https' -fsSL -o "$tmp/$deb" "$base/$deb" \
    || die "release $tag has no $deb (it may be older than the apt packages). See $releases"
  curl --proto '=https' -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" \
    || die "couldn't download $base/SHA256SUMS"
  chmod 0644 "$tmp/$deb"

  # The package must be listed in SHA256SUMS, by its exact name, and match that checksum, or
  # nothing gets installed.
  awk -v f="$deb" '$2 == f' "$tmp/SHA256SUMS" > "$tmp/want"
  [ -s "$tmp/want" ] || die "$deb isn't in the release's checksums, so I'm not installing it"
  (cd "$tmp" && sha256sum -c want >/dev/null 2>&1) || die "checksum mismatch for $deb, so I'm not installing it"

  # apt needs root, so use sudo when this isn't already running as root. apt gets no stdin,
  # so nothing it asks can swallow input meant for anything else.
  sudo=""
  if [ "$(id -u)" -ne 0 ]; then
    command -v sudo >/dev/null 2>&1 || die "run this as root, or install sudo"
    sudo=sudo
  fi
  say "installing with apt"
  $sudo apt-get install -y "$tmp/$deb" </dev/null || die "apt couldn't install $deb"

  say "installed $(reses --version 2>/dev/null || echo "reses $version"). Run reses to open the inbox."
}

# Progress and errors both go to stderr, prefixed so they stand out from apt's own output.
say() { printf 'reses installer: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }

main "$@"
