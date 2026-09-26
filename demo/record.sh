#!/usr/bin/env bash
# Record docs/demo.gif from demo/demo.tape against a throwaway, seeded MinIO. Everything runs
# in Docker: the release build in the CI image, MinIO on a private per-run network, the seed
# through curl in the CI image, and the recording in the official vhs image.
#
# The app only ever sees a credentials file with MinIO's fixed test keys, a config and a HOME
# that live in a scratch directory mounted into the recording container, and an endpoint that
# only resolves on the per-run network. Nothing here reads ~/.aws or talks to real AWS.
#
# The recording is checked before it replaces docs/demo.gif: vhs also writes the frames as text,
# and this script fails unless they show the inbox rows, its columns and the opened message.
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

PLATFORM="linux/arm64"
# vhs v0.12.1 and MinIO RELEASE.2026-09-22T19-25-18Z (the same pin as ci-docker.sh --integration).
VHS_IMAGE="ghcr.io/charmbracelet/vhs@sha256:ea49a6a1c529be83153e88321892b5585964418f0b9055e8c1e0d732194234a3"
MINIO_IMAGE="cgr.dev/chainguard/minio@sha256:bd014394a80898e68c149f2311fdf8d5a2c2f3bb2c33b9327ae6d02b4b065ae1"
# The tag ci-docker.sh gives the CI image. The build step below makes sure it exists.
CI_IMAGE="reses-ci:$(shasum -a 256 "$REPO_ROOT/docker/ci.Dockerfile" | cut -c1-12)"

# MinIO's fixed test keys. They only open the throwaway MinIO this script starts.
KEY=resesadmin
SECRET=resesadmin-secret

RUN="reses-demo-$$-$(date +%s)"
NET="$RUN-net"
MINIO="$RUN-minio"
WORK="$REPO_ROOT/demo/.work"

cleanup() {
  docker rm -f "$MINIO" >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT
rm -rf "$WORK"
mkdir -p "$WORK/bin" "$WORK/home/.aws" "$WORK/home/.config/reses"

echo "==> building reses"
scripts/ci-docker.sh --exec 'cargo build --release --locked && cp /target/release/reses demo/.work/bin/reses'

echo "==> starting MinIO"
docker network create "$NET" >/dev/null
docker run -d --rm --platform "$PLATFORM" --name "$MINIO" --network "$NET" --network-alias minio \
  -e MINIO_ROOT_USER="$KEY" -e MINIO_ROOT_PASSWORD="$SECRET" \
  "$MINIO_IMAGE" server /tmp/minio-data >/dev/null

echo "==> seeding"
docker run --rm --platform "$PLATFORM" --network "$NET" \
  -e S3_ENDPOINT=http://minio:9000 -e S3_KEY="$KEY" -e S3_SECRET="$SECRET" \
  -v "$REPO_ROOT/demo/seed.sh:/seed.sh:ro" "$CI_IMAGE" bash /seed.sh

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
docker run --rm --platform "$PLATFORM" --network "$NET" \
  -e HOME=/home/demo \
  -e AWS_SHARED_CREDENTIALS_FILE=/home/demo/.aws/credentials \
  -e AWS_CONFIG_FILE=/home/demo/.aws/config \
  -e RESES_CONFIG=/home/demo/.config/reses/config.toml \
  -e RESES_S3_ENDPOINT=http://minio:9000 \
  -e PATH=/demo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
  -v "$WORK:/demo" -v "$WORK/home:/home/demo" -w /demo \
  "$VHS_IMAGE" demo.tape

echo "==> checking the recording"
docker run --rm --platform "$PLATFORM" -v "$REPO_ROOT/demo:/check:ro" -v "$WORK:/demo:ro" \
  "$CI_IMAGE" bash /check/check-frames.sh /demo/demo.txt

mkdir -p docs
cp "$WORK/demo.gif" docs/demo.gif
echo "wrote docs/demo.gif ($(wc -c <docs/demo.gif | tr -d ' ') bytes)"
