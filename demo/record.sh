#!/usr/bin/env bash
# Record docs/demo.gif from demo/demo.tape against a throwaway, seeded MinIO. Every container
# runs on the UGREEN NAS through scripts/nas-lib.sh: the release build in the CI image, MinIO on
# a private per-run network, the seed through curl in the CI image, the recording in the
# official vhs image, and the frame check. Only the finished GIF (and, if asked, the text
# snapshots) comes back to this machine.
#
# The app only ever sees a credentials file with MinIO's fixed test keys, a config and a HOME
# that live in a per-run scratch directory on the NAS, and an endpoint that only resolves on the
# per-run network. Nothing here reads ~/.aws or talks to real AWS.
#
# The recording is checked before it replaces docs/demo.gif: vhs also writes a text snapshot
# after every tape command, and demo/check-frames.sh fails unless those snapshots show every
# seeded inbox row, the columns and the opened message, with no error text.
#
# RESES_LANE names the NAS scratch directory and cargo target volume, as for ci-docker.sh. It
# defaults to "demo" here, so recording never shares build output with the main gate.
#
# RESES_DEMO_FRAMES, when set, is a path to copy the text snapshots to (for example to refresh
# tests/fixtures/demo/good.txt), with the GIF next to them. Both are copied before the check, so a
# failed run can be looked at.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"
export RESES_LANE="${RESES_LANE:-demo}"
# shellcheck source=../scripts/nas-lib.sh
. "$REPO_ROOT/scripts/nas-lib.sh"
PLATFORM="$NAS_PLATFORM"

VHS_IMAGE="$NAS_VHS_IMAGE"
MINIO_IMAGE="$NAS_MINIO_IMAGE"

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
# The lane's pushed tree on the NAS, and this run's scratch directory inside it (so the build
# container, which mounts only the lane tree, can write the binary there).
TREE="$NAS_SCRATCH/$RESES_LANE"
WORK_REL="$RESES_LANE/demo/.work/$RUN"
WORK="$NAS_SCRATCH/$WORK_REL"
# The local staging directory lives in the repo's gitignored tmp/, never in the system temp dir.
mkdir -p "$REPO_ROOT/tmp"
LOCAL="$(mktemp -d "$REPO_ROOT/tmp/demo.XXXXXX")"
# ci-docker.sh would scrub the lane when each call ends, and the later steps still need it, so
# I keep it and scrub the whole lane myself at the end.
export RESES_KEEP_TREE=1
# I hold the lane for the whole recording, and the ci-docker.sh calls below reuse the hold.
nas_lock "$RESES_LANE"

cleanup() {
  dk rm -f "$SEED" "$VHS" "$CHECK" "$MINIO" >/dev/null 2>&1 || true
  dk network rm "$NET" >/dev/null 2>&1 || true
  nas_scrub "$RESES_LANE"
  nas_unlock
  rm -rf "$LOCAL"
}
trap cleanup EXIT INT TERM

# The build pushes the tree and leaves the binary in this run's directory on the NAS.
echo "==> building reses on the NAS"
scripts/ci-docker.sh --exec "cargo build --release --locked && mkdir -p $(printf %q "$WORK/bin") && cp /target/release/reses $(printf %q "$WORK/bin/reses")"
# Asked after the build, which is what makes sure the image exists.
CI_IMAGE="$(scripts/ci-docker.sh --image-tag)"

# The app's HOME, credentials and config, plus the tape, go over in one tar.
mkdir -p "$LOCAL/home/.aws" "$LOCAL/home/.config/reses"
cat >"$LOCAL/home/.aws/credentials" <<EOF
[default]
aws_access_key_id = $KEY
aws_secret_access_key = $SECRET

[mail-archive]
aws_access_key_id = $KEY
aws_secret_access_key = $SECRET
EOF
chmod 600 "$LOCAL/home/.aws/credentials"
printf '[default]\nregion = us-east-1\n\n[profile mail-archive]\nregion = eu-west-1\n' >"$LOCAL/home/.aws/config"
cp demo/demo.tape "$LOCAL/demo.tape"
(cd "$LOCAL" && tar -cf - home demo.tape) | nas tar -xf - -C "$WORK"

echo "==> starting MinIO"
dk network create "$NET" >/dev/null
dk run -d --rm --platform "$PLATFORM" --name "$MINIO" --network "$NET" --network-alias minio \
  -e MINIO_ROOT_USER="$KEY" -e MINIO_ROOT_PASSWORD="$SECRET" \
  "$MINIO_IMAGE" server /tmp/minio-data >/dev/null

echo "==> seeding"
dk run --rm --name "$SEED" --platform "$PLATFORM" --network "$NET" \
  -e S3_ENDPOINT=http://minio:9000 -e S3_KEY="$KEY" -e S3_SECRET="$SECRET" \
  -e MESSAGES=/demo/messages.tsv \
  -v "$TREE/demo:/demo:ro" "$CI_IMAGE" bash /demo/seed.sh

# Chromium inside vhs dies at start under Docker's default 64MB /dev/shm, so it gets more. Even
# then it sometimes misses vhs's start deadline while the NAS is busy with other builds, so I
# retry that one failure, and only that one, a couple of times.
record() {
  dk run --rm --name "$VHS" --shm-size=1g --platform "$PLATFORM" --network "$NET" \
    -e HOME=/home/demo \
    -e AWS_SHARED_CREDENTIALS_FILE=/home/demo/.aws/credentials \
    -e AWS_CONFIG_FILE=/home/demo/.aws/config \
    -e RESES_CONFIG=/home/demo/.config/reses/config.toml \
    -e RESES_S3_ENDPOINT=http://minio:9000 \
    -e PATH=/demo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    -v "$WORK:/demo" -v "$WORK/home:/home/demo" -w /demo \
    "$VHS_IMAGE" demo.tape
}
for attempt in 1 2 3; do
  echo "==> recording (attempt $attempt)"
  if out="$(record 2>&1)"; then
    printf '%s\n' "$out"
    break
  fi
  printf '%s\n' "$out" >&2
  if [ "$attempt" = 3 ] || ! grep -q 'could not start browser' <<<"$out"; then
    exit 1
  fi
done

# Brought back, and copied out before the check, so a failed recording can still be read.
nas_fetch "$WORK" "$LOCAL/out" demo.gif demo.txt
if [ -n "${RESES_DEMO_FRAMES:-}" ]; then
  cp "$LOCAL/out/demo.txt" "$RESES_DEMO_FRAMES"
  cp "$LOCAL/out/demo.gif" "${RESES_DEMO_FRAMES%.txt}.gif"
fi
echo "==> checking the recording"
dk run --rm --name "$CHECK" --platform "$PLATFORM" \
  -v "$TREE/demo:/check:ro" -v "$WORK:/demo:ro" \
  "$CI_IMAGE" bash /check/check-frames.sh /demo/demo.txt /check/messages.tsv

# Written next to the old GIF and moved over it, so a concurrent or interrupted run never
# leaves a half-written docs/demo.gif.
mkdir -p docs
GIF_TMP="$(mktemp "$REPO_ROOT/docs/.demo.gif.XXXXXX")"
cp "$LOCAL/out/demo.gif" "$GIF_TMP"
chmod 644 "$GIF_TMP"
mv "$GIF_TMP" docs/demo.gif
echo "wrote docs/demo.gif ($(wc -c <docs/demo.gif | tr -d ' ') bytes)"
