#!/usr/bin/env bash
# Record docs/demo.gif from demo/demo.tape against a throwaway, seeded MinIO. Everything runs
# in Docker: the release build in the CI image, MinIO on a private per-run network, the seed
# through curl in the CI image, and the recording in the official vhs image.
#
# The app only ever sees a credentials file with MinIO's fixed test keys, a config and a HOME
# that live in a per-run scratch directory mounted into the recording container, and an endpoint
# that only resolves on the per-run network. Nothing here reads ~/.aws or talks to real AWS.
#
# The recording is checked before it replaces docs/demo.gif: vhs also writes a text snapshot
# after every tape command, and demo/check-frames.sh fails unless those snapshots show every
# seeded inbox row, the columns and the opened message, with no error text.
#
# RESES_LANE names the cargo target volume, as for ci-docker.sh. It defaults to "demo" here,
# so recording never shares build output with the main gate.
#
# RESES_DEMO_FRAMES, when set, is a path to copy the text snapshots to (for example to refresh
# tests/fixtures/demo/good.txt), with the GIF next to them. Both are copied before the check, so a
# failed run can be looked at.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"
export RESES_LANE="${RESES_LANE:-demo}"

PLATFORM="linux/arm64"
# vhs v0.12.1 and MinIO RELEASE.2026-09-22T19-25-18Z (the same pin as ci-docker.sh --integration).
VHS_IMAGE="ghcr.io/charmbracelet/vhs@sha256:ea49a6a1c529be83153e88321892b5585964418f0b9055e8c1e0d732194234a3"
MINIO_IMAGE="cgr.dev/chainguard/minio@sha256:bd014394a80898e68c149f2311fdf8d5a2c2f3bb2c33b9327ae6d02b4b065ae1"

# MinIO's fixed test keys. They only open the throwaway MinIO this script starts.
KEY=resesadmin
SECRET=resesadmin-secret

# Every container, the network and the scratch directory are named for this run, so two runs
# never collide and cleanup only ever touches what this one made.
RUN="reses-demo-$$-$(date +%s)"
NET="$RUN-net"
MINIO="$RUN-minio"
SEED="$RUN-seed"
VHS="$RUN-vhs"
CHECK="$RUN-check"
mkdir -p "$REPO_ROOT/demo/.work"
WORK="$(mktemp -d "$REPO_ROOT/demo/.work/run.XXXXXX")"
GIF_TMP=""

cleanup() {
  docker rm -f "$SEED" "$VHS" "$CHECK" "$MINIO" >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
  rm -rf "$WORK"
  [ -z "$GIF_TMP" ] || rm -f "$GIF_TMP"
}
trap cleanup EXIT
mkdir -p "$WORK/bin" "$WORK/home/.aws" "$WORK/home/.config/reses"

echo "==> building reses"
scripts/ci-docker.sh --exec "cargo build --release --locked && cp /target/release/reses $(printf %q "$WORK/bin/reses")"
# Asked after the build, which is what makes sure the image exists.
CI_IMAGE="$(scripts/ci-docker.sh --image-tag)"

echo "==> starting MinIO"
docker network create "$NET" >/dev/null
docker run -d --rm --platform "$PLATFORM" --name "$MINIO" --network "$NET" --network-alias minio \
  -e MINIO_ROOT_USER="$KEY" -e MINIO_ROOT_PASSWORD="$SECRET" \
  "$MINIO_IMAGE" server /tmp/minio-data >/dev/null

echo "==> seeding"
docker run --rm --name "$SEED" --platform "$PLATFORM" --network "$NET" \
  -e S3_ENDPOINT=http://minio:9000 -e S3_KEY="$KEY" -e S3_SECRET="$SECRET" \
  -e MESSAGES=/demo/messages.tsv \
  -v "$REPO_ROOT/demo:/demo:ro" "$CI_IMAGE" bash /demo/seed.sh

cat >"$WORK/home/.aws/credentials" <<EOF
[default]
aws_access_key_id = $KEY
aws_secret_access_key = $SECRET

[mail-archive]
aws_access_key_id = $KEY
aws_secret_access_key = $SECRET
EOF
chmod 600 "$WORK/home/.aws/credentials"
printf '[default]\nregion = us-east-1\n\n[profile mail-archive]\nregion = eu-west-1\n' >"$WORK/home/.aws/config"

echo "==> recording"
cp demo/demo.tape "$WORK/demo.tape"
docker run --rm --name "$VHS" --platform "$PLATFORM" --network "$NET" \
  -e HOME=/home/demo \
  -e AWS_SHARED_CREDENTIALS_FILE=/home/demo/.aws/credentials \
  -e AWS_CONFIG_FILE=/home/demo/.aws/config \
  -e RESES_CONFIG=/home/demo/.config/reses/config.toml \
  -e RESES_S3_ENDPOINT=http://minio:9000 \
  -e PATH=/demo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
  -v "$WORK:/demo" -v "$WORK/home:/home/demo" -w /demo \
  "$VHS_IMAGE" demo.tape

# Copied before the check, so a failed recording can still be read.
if [ -n "${RESES_DEMO_FRAMES:-}" ]; then
  cp "$WORK/demo.txt" "$RESES_DEMO_FRAMES"
  cp "$WORK/demo.gif" "${RESES_DEMO_FRAMES%.txt}.gif"
fi
echo "==> checking the recording"
docker run --rm --name "$CHECK" --platform "$PLATFORM" \
  -v "$REPO_ROOT/demo:/check:ro" -v "$WORK:/demo:ro" \
  "$CI_IMAGE" bash /check/check-frames.sh /demo/demo.txt /check/messages.tsv

# Written next to the old GIF and moved over it, so a concurrent or interrupted run never
# leaves a half-written docs/demo.gif.
mkdir -p docs
GIF_TMP="$(mktemp "$REPO_ROOT/docs/.demo.gif.XXXXXX")"
cp "$WORK/demo.gif" "$GIF_TMP"
chmod 644 "$GIF_TMP"
mv "$GIF_TMP" docs/demo.gif
GIF_TMP=""
echo "wrote docs/demo.gif ($(wc -c <docs/demo.gif | tr -d ' ') bytes)"
