#!/usr/bin/env bash
# Render the committed brand assets from the SVGs in assets/brand/source/. Everything runs in
# Docker on the NAS (scripts/nas-lib.sh), so neither Poppins nor Inkscape is ever needed on the host, and the build never needs
# either: the outputs are committed.
#
#   scripts/render-brand.sh
#
# What it writes into assets/brand/:
#   reSES-logo.svg, reSES-wordmark.svg, reSES-cubes.svg
#       the sources with their "re:SES" text converted to outlines (Poppins, SIL OFL)
#   reSES-logo-light.svg, reSES-wordmark-light.svg, reSES-cubes-light.svg
#       light variants for dark backgrounds: light text, and the white gaps between the cubes
#       cut out with a mask instead, so they show the background rather than a white halo
#   tui/reses-header.png, tui/reses-header-light.png
#       the cubes left of the wordmark, one horizontal logo for the TUI header
#   Poppins-OFL.txt
#       the font's license, since its glyph outlines are now in the SVGs
set -euo pipefail

# The renders run on the NAS, like every other container this repo starts.
PLATFORM="linux/amd64"

# Poppins from google/fonts, pinned to a commit and checked by hash.
FONTS_COMMIT="8b0a1d0f5983c89bc2b93f1b5fb55f9e252744b5"
FONTS_URL="https://raw.githubusercontent.com/google/fonts/${FONTS_COMMIT}/ofl/poppins"
REGULAR_SHA256="7e65201e9b79159e2300267cc885e16c8dcef2424cdfa09a29bfb0980a94a7ba"
EXTRABOLD_SHA256="f2ab17c1a63a0ecc12c2461848fc8a469395e3cd2d641803e889c643d9f958e1"
OFL_SHA256="6be04893d770899a015649c7aa3b582f871b272f8747a92b78b17c3e5c8b2573"

DOCKERFILE='FROM debian@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251
RUN apt-get update \
 && apt-get install -y --no-install-recommends inkscape imagemagick fontconfig curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*'

if [ "${1:-}" != "--inside" ]; then
  REPO_ROOT="$(git rev-parse --show-toplevel)"
  export RESES_LANE="${RESES_LANE:-brand}"
  # shellcheck source=nas-lib.sh
  . "$REPO_ROOT/scripts/nas-lib.sh"
  PLATFORM="$NAS_PLATFORM"
  W="$NAS_SCRATCH/$RESES_LANE"
  IMAGE="reses-brand:$(printf '%s' "$DOCKERFILE" | shasum -a 256 | cut -c1-12)"
  RUN_NAME="reses-$RESES_LANE-$$"
  mkdir -p "$REPO_ROOT/tmp"
  BACK="$(mktemp -d "$REPO_ROOT/tmp/brand.XXXXXX")"
  trap 'dk rm -f "$RUN_NAME" >/dev/null 2>&1 || true; [ -z "${NAS_LOCK_MINE:-}" ] || nas_scrub "$RESES_LANE"; nas_unlock; rm -rf "$BACK"' EXIT INT TERM

  nas_push "$RESES_LANE"
  if ! dk image inspect "$IMAGE" >/dev/null 2>&1; then
    printf '%s\n' "$DOCKERFILE" | "${NAS_SSH[@]}" "docker build --platform $PLATFORM -t $IMAGE -"
  fi
  dk run --rm --name "$RUN_NAME" --platform "$PLATFORM" \
    -e FONTS_URL="$FONTS_URL" -e REGULAR_SHA256="$REGULAR_SHA256" \
    -e EXTRABOLD_SHA256="$EXTRABOLD_SHA256" -e OFL_SHA256="$OFL_SHA256" \
    -v "$W:/repo" -w /repo "$IMAGE" bash scripts/render-brand.sh --inside
  # Only the rendered assets come back, and they land beside the old ones before moving over.
  nas_fetch "$W" "$BACK" assets/brand
  cp -R "$BACK/assets/brand/." "$REPO_ROOT/assets/brand/"
  exit 0
fi

# ---- inside the container from here on ----
BRAND=/repo/assets/brand
WORK=$(mktemp -d)
mkdir -p "$WORK/fonts" "$WORK/src" "$BRAND/tui"

fetch() {
  curl -fsSL "$FONTS_URL/$1" -o "$WORK/fonts/$1"
  local got
  got=$(sha256sum "$WORK/fonts/$1" | cut -d' ' -f1)
  if [ "$got" != "$2" ]; then
    echo "$1: sha256 $got, expected $2" >&2
    exit 1
  fi
}
fetch Poppins-Regular.ttf "$REGULAR_SHA256"
fetch Poppins-ExtraBold.ttf "$EXTRABOLD_SHA256"
fetch OFL.txt "$OFL_SHA256"
mkdir -p ~/.local/share/fonts
cp "$WORK"/fonts/*.ttf ~/.local/share/fonts/
fc-cache -f >/dev/null
# Inkscape falls back to another font without a word, so make sure fontconfig has both weights.
fc-list | grep -q 'Poppins-Regular.ttf' && fc-list | grep -q 'Poppins-ExtraBold.ttf' \
  || { echo "Poppins did not install" >&2; exit 1; }
cp "$WORK/fonts/OFL.txt" "$BRAND/Poppins-OFL.txt"

# Replace $2 with $3 in file $1, failing unless it matched exactly $4 times, so a changed
# source can't quietly produce a half-edited variant.
edit() {
  local n
  n=$(grep -cF -- "$2" "$1" || true)
  if [ "$n" != "$4" ]; then
    echo "$1: expected $4 line(s) with $2, found $n" >&2
    exit 1
  fi
  local from to
  from=$(printf '%s' "$2" | sed 's/[][\/.*^$]/\\&/g')
  to=$(printf '%s' "$3" | sed 's/[\/&]/\\&/g')
  sed -i "s/$from/$to/g" "$1"
}

for name in logo wordmark cubes; do
  # Drop any provenance metadata the source carries: the outputs are what gets published.
  sed -e 's|<metadata>.*</metadata>||' -e 's| xmlns:c2pa="[^"]*"||' \
    "$BRAND/source/reSES-$name.svg" > "$WORK/src/reSES-$name.svg"
done

# The light variants. Text goes light, and the cubes lose their white separating silhouettes:
# masks cut the same gaps out of the cubes behind, so the gaps are transparent.
LIGHT_TEXT="#F4F7FB"
SIL='<use href="#sil" xlink:href="#sil" fill="#FFFFFF" stroke="#FFFFFF" stroke-width="30"></use>'
MASKS='<mask id="gap-top" maskUnits="userSpaceOnUse" x="-2000" y="-2000" width="4000" height="4000"><rect x="-2000" y="-2000" width="4000" height="4000" fill="#FFFFFF"/><use href="#sil" xlink:href="#sil" transform="translate(-130 212)" fill="#000000" stroke="#000000" stroke-width="30" stroke-linejoin="round"/><use href="#sil" xlink:href="#sil" transform="translate(140 212)" fill="#000000" stroke="#000000" stroke-width="30" stroke-linejoin="round"/></mask><mask id="gap-left" maskUnits="userSpaceOnUse" x="-2000" y="-2000" width="4000" height="4000"><rect x="-2000" y="-2000" width="4000" height="4000" fill="#FFFFFF"/><use href="#sil" xlink:href="#sil" transform="translate(270 0)" fill="#000000" stroke="#000000" stroke-width="30" stroke-linejoin="round"/></mask>'
for name in logo cubes; do
  f="$WORK/src/reSES-$name-light.svg"
  cp "$WORK/src/reSES-$name.svg" "$f"
  edit "$f" "$SIL" "" 2
  edit "$f" '</defs>' "$MASKS</defs>" 1
  edit "$f" '<g transform="translate(455 20)">' '<g transform="translate(455 20)" mask="url(#gap-top)">' 1
  edit "$f" '<g transform="translate(325 232)">' '<g transform="translate(325 232)" mask="url(#gap-left)">' 1
done
for name in logo wordmark; do
  f="$WORK/src/reSES-$name-light.svg"
  [ -f "$f" ] || cp "$WORK/src/reSES-$name.svg" "$f"
  edit "$f" 'fill="#141B2B"' "fill=\"$LIGHT_TEXT\"" 2
done

# Outlines. Plain SVG keeps Inkscape's own namespaces out of the files.
for f in "$WORK"/src/*.svg; do
  out="$BRAND/$(basename "$f")"
  inkscape --export-text-to-path --export-plain-svg --export-type=svg -o "$out" "$f" 2>/dev/null
  if grep -q '<text' "$out"; then
    echo "$out still has live text" >&2
    exit 1
  fi
done

# The TUI header: cubes, a gap, then the wordmark at about half the cubes' height.
for suffix in "" "-light"; do
  inkscape --export-type=png --export-height=192 -o "$WORK/cubes$suffix.png" \
    "$BRAND/reSES-cubes$suffix.svg" 2>/dev/null
  inkscape --export-type=png --export-height=104 -o "$WORK/wordmark$suffix.png" \
    "$BRAND/reSES-wordmark$suffix.svg" 2>/dev/null
  convert -background none "$WORK/cubes$suffix.png" -size 36x1 xc:none \
    "$WORK/wordmark$suffix.png" -gravity center +append -strip \
    -define png:exclude-chunks=date,time "$BRAND/tui/reses-header$suffix.png"
done

# Nothing published may carry provenance metadata.
if grep -rliE 'c2pa|<metadata' "$BRAND"/*.svg "$BRAND"/source/*.svg "$BRAND"/tui/*.png; then
  echo "provenance metadata survived into the files above" >&2
  exit 1
fi
ls -l "$BRAND" "$BRAND/tui"
