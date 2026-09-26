#!/usr/bin/env bash
# Run reses builds and tests in the CI image on the UGREEN NAS. Nothing here touches this Mac's
# toolchain or its Docker.
#
#   scripts/ci-docker.sh                full gate: the Check, MSRV, Docs, Test, Deny and Linux
#                                       static build jobs of ci.yml
#   scripts/ci-docker.sh --exec CMD     run CMD in the container (e.g. "cargo test mail")
#   scripts/ci-docker.sh --shell        interactive shell
#   scripts/ci-docker.sh --darwin       build the macOS release binary into dist/ on this Mac
#   scripts/ci-docker.sh --integration  run the MinIO-backed S3 tests
#   scripts/ci-docker.sh --image-tag    print the CI image tag (demo/record.sh uses it)
#   scripts/ci-docker.sh --nas-clean    remove every reses container, image, volume and scratch
#                                       directory from the NAS
#   scripts/ci-docker.sh --nas-unlock LANE  release a lock a killed run left on LANE
#
# The NAS (native x86_64) only ever runs ssh, docker, and tar into one scratch directory,
# ~/workspace/reses-ci/<lane>. Every job runs in a --rm container there. Each run I push the
# working tree fresh: tracked and untracked files but not ignored ones, with timestamps left
# at "now" so cargo never mistakes an old build for a current one. The NAS has no git, so the
# container rebuilds a git index from exactly those files for tests that ask git. Mounts are
# path-identical (-v P:P), so containers that start other containers see real files. No
# token or key is ever sent to the NAS.
#
# RESES_LANE names the scratch directory and the cargo target volume, so parallel worktrees
# never share build output. RESES_NAS overrides the host.
#
# Every container gets RUSTFLAGS=-Dwarnings, the value ci.yml sets for all its jobs, so the gate
# compiles exactly what CI compiles and a warning fails here the way it fails there.
# tests/ci_parity.rs compares the two environments.
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
LANE="${RESES_LANE:-main}"
# Tagged by a hash of the Dockerfile, so editing the Dockerfile builds a fresh image instead of
# silently reusing the old one.
IMAGE="reses-ci:$(shasum -a 256 "$REPO_ROOT/docker/ci.Dockerfile" | cut -c1-12)"

if [ "${1:-}" = "--image-tag" ]; then
  echo "$IMAGE"
  exit 0
fi

# shellcheck source=scripts/nas-lib.sh
. "$REPO_ROOT/scripts/nas-lib.sh"
PLATFORM="$NAS_PLATFORM"
TARGET_VOL="reses-target-${LANE}"
CARGO_HOME_VOL="reses-cargo-home"

W="$NAS_SCRATCH/$LANE"

# Built on first use, so --nas-unlock and --nas-clean never wait for (or fail on) an image build.
ensure_image() {
  if ! dk image inspect "$IMAGE" >/dev/null 2>&1; then
    "${NAS_SSH[@]}" "docker build --platform $PLATFORM -t $IMAGE -" < "$REPO_ROOT/docker/ci.Dockerfile"
  fi
}

# Run a command in the CI image with the pushed tree mounted at its own path.
run() {
  local tty=()
  ensure_image
  [ "${INTERACTIVE:-}" = 1 ] && tty=(-it)
  # The NAS has no git, so I rebuild an index of exactly the files git tracks here.
  local cmd='set -e
[ -d .git ] || { git init -q && GIT_LITERAL_PATHSPECS=1 git add -f --pathspec-from-file=.reses-tracked --pathspec-file-nul; }
set +e
'"$1"
  local args=(docker run --rm --name "$RUN_NAME" ${tty[@]+"${tty[@]}"} --platform "$PLATFORM"
    --cpus "${RESES_CPUS:-2}" --memory "${RESES_MEMORY:-3g}"
    -e CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-auto}" -e CARGO_BUILD_JOBS="${RESES_CPUS:-2}"
    -e CARGO_TARGET_DIR=/target -e CARGO_HOME=/cargo-home -e RUSTFLAGS=-Dwarnings
    -e GIT_CONFIG_COUNT=1 -e GIT_CONFIG_KEY_0=safe.directory -e GIT_CONFIG_VALUE_0='*'
    ${EXTRA_DOCKER_ARGS[@]+"${EXTRA_DOCKER_ARGS[@]}"}
    -v "$CARGO_HOME_VOL:/cargo-home"
    -v "$TARGET_VOL:/target"
    -v "$W:$W" -w "$W"
    "$IMAGE" bash -c "$cmd")
  if [ ${#tty[@]} -gt 0 ]; then
    ssh -t -o BatchMode=yes -o LogLevel=ERROR "$NAS" "$(printf '%q ' "${args[@]}")"
  else
    # In the background and waited on, so a TERM reaches the trap at once instead of after the
    # whole remote job.
    nas "${args[@]}" &
    wait $!
  fi
}
EXTRA_DOCKER_ARGS=()

# Every container this run starts is named for it, so an interrupted run can be cleaned up and
# --nas-clean can always find it. The lane's pushed tree goes at the end too, unless a caller
# (demo/record.sh) still needs it.
nas_check_lane "$LANE"
RUN_NAME="reses-$LANE-$$"
# The EXIT trap: remove this run's container, clear the lane if it's still mine, let go of the lock.
finish() {
  dk rm -f "$RUN_NAME" >/dev/null 2>&1 || true
  # Only a run that holds the lane clears it, so one turned away by the lock never wipes the
  # tree of the run that has it.
  if [ -z "${RESES_KEEP_TREE:-}" ] && nas_lock_still_mine; then nas_scrub "$LANE" || true; fi
  nas_unlock
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# The gate. cargo deny fetches the advisory database each run, so it needs the network. The
# musl build is the static Linux binary the release ships, for the arch of the container (the
# NAS image is x86_64; ci.yml builds both), checked the way the release checks it.
GATE='set -e
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo check --all-targets --locked
RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links -D rustdoc::redundant_explicit_links" cargo doc --no-deps --locked
cargo test --locked --no-fail-fast
cargo deny check advisories bans licenses sources
MUSL_TARGET="$(uname -m)-unknown-linux-musl"
TARGET_CC=musl-gcc cargo build --release --locked --target "$MUSL_TARGET"
scripts/check-static.sh "/target/$MUSL_TARGET/release/reses"
scripts/check-goldens.sh "/target/$MUSL_TARGET/release/reses"'

case "${1:-}" in
  "") nas_push "$LANE"; run "$GATE" ;;
  --exec) shift; nas_push "$LANE"; run "$*" ;;
  --shell) nas_push "$LANE"; INTERACTIVE=1 run bash ;;
  --darwin)
    nas_push "$LANE"
    # place-binary.sh rather than cp, both on the NAS and back here: the Mac runs the result,
    # and a cp over the previous build leaves a binary macOS kills at exec (#15).
    run 'set -e
cargo zigbuild --release --locked --target aarch64-apple-darwin
mkdir -p dist && scripts/place-binary.sh /target/aarch64-apple-darwin/release/reses dist/reses-aarch64-apple-darwin
ls -l dist/'
    mkdir -p "$REPO_ROOT/tmp"
    back="$(mktemp -d "$REPO_ROOT/tmp/darwin.XXXXXX")"
    nas tar -cf - -C "$W" dist/reses-aarch64-apple-darwin | tar -xf - -C "$back"
    mkdir -p "$REPO_ROOT/dist"
    "$REPO_ROOT/scripts/place-binary.sh" "$back/dist/reses-aarch64-apple-darwin" "$REPO_ROOT/dist/reses-aarch64-apple-darwin"
    rm -rf "$back"
    ls -l "$REPO_ROOT/dist/" ;;
  --integration)
    # The lane is taken before MinIO or the network exist, and both are named for this run, so a
    # second run turned away by the lock never touches the first one's.
    nas_lock "$LANE"
    NET="reses-it-$LANE-$$"
    MINIO="reses-minio-$LANE-$$"
    # minio/minio is no longer pullable from Docker Hub, so I use Chainguard's build, pinned
    # by digest (MinIO RELEASE.2026-09-22T19-25-18Z). It runs as a non-root user, so the
    # data directory lives under /tmp.
    MINIO_IMAGE="$NAS_MINIO_IMAGE"
    # Clean up first thing, so a failed start never leaves the network or container behind.
    trap 'dk rm -f "$MINIO" >/dev/null 2>&1 || true; dk network rm "$NET" >/dev/null 2>&1 || true; finish' EXIT
    dk network create "$NET" >/dev/null 2>&1 || true
    dk rm -f "$MINIO" >/dev/null 2>&1 || true
    dk run -d --rm --platform "$PLATFORM" --name "$MINIO" --network "$NET" \
      -e MINIO_ROOT_USER=resesadmin -e MINIO_ROOT_PASSWORD=resesadmin-secret \
      "$MINIO_IMAGE" server /tmp/minio-data >/dev/null
    EXTRA_DOCKER_ARGS=(--network "$NET"
      -e RESES_TEST_S3_ENDPOINT="http://$MINIO:9000"
      -e RESES_TEST_S3_ACCESS_KEY=resesadmin -e RESES_TEST_S3_SECRET_KEY=resesadmin-secret)
    nas_push "$LANE"
    # MinIO takes a moment to listen; wait for its health check rather than racing it.
    run 'set -e
for i in $(seq 1 60); do
  curl -fs -o /dev/null "$RESES_TEST_S3_ENDPOINT/minio/health/live" && break
  [ "$i" = 60 ] && { echo "MinIO never came up" >&2; exit 1; }
  sleep 1
done
cargo test --locked --no-fail-fast -- --ignored --test-threads=1' ;;
  --nas-unlock)
    # For a lock a killed run left behind. The lock goes, and so does any container that run
    # left going, so it can't keep writing into the next run's tree. The tree itself stays.
    trap - EXIT INT TERM
    nas_check_lane "${2:-}"
    names="$(dk ps -a --format '{{.Names}}' | grep -E "^reses-$2-[0-9]+\$" || true)"
    [ -z "$names" ] || dk rm -f $names >/dev/null
    nas_scrub ".lock-$2"
    echo "unlocked lane '$2' on $NAS" ;;
  --nas-clean)
    # Everything reses put on the NAS, and nothing that belongs to anyone else.
    # Names are matched on their start here rather than by docker's substring filter, and images
    # by their exact repository or pinned digest, so nothing that merely mentions "reses" goes.
    # BuildKit's cache is shared with anything else building there, so I leave it alone.
    trap - EXIT INT TERM
    held="$(nas sh -c "cd '$NAS_SCRATCH' 2>/dev/null && ls -d .lock-* 2>/dev/null" || true)"
    if [ -n "$held" ] && [ "${2:-}" != --force ]; then
      echo "these lanes are in use on $NAS, so I'm not cleaning (add --force if those runs are gone):" >&2
      printf '%s\n' "$held" | sed 's/^\.lock-/  /' >&2
      exit 1
    fi
    # One docker call per kind, so no loop's stdin ends up read by ssh.
    names="$(dk ps -a --format '{{.Names}}' | grep -E '^reses-' || true)"
    [ -z "$names" ] || dk rm -f $names >/dev/null
    names="$(dk network ls --format '{{.Name}}' | grep -E '^reses-' || true)"
    [ -z "$names" ] || dk network rm $names >/dev/null
    names="$(dk volume ls --format '{{.Name}}' | grep -E '^reses-' || true)"
    [ -z "$names" ] || dk volume rm $names >/dev/null
    names="$(dk image ls --format '{{.Repository}}:{{.Tag}}' | grep -E '^reses-(ci|brand):' || true)"
    [ -z "$names" ] || dk rmi $names >/dev/null
    # The pinned images reses pulled or built from. A digest reference only goes if nothing else
    # tags that image, so an alpine:3 that was already there stays.
    for i in "$NAS_VHS_IMAGE" "$NAS_MINIO_IMAGE" \
      "$(sed -n 's/^FROM \([^ ]*\).*/\1/p' "$REPO_ROOT/docker/ci.Dockerfile" | head -1)" \
      "$(sed -n "s/^DOCKERFILE='FROM \([^ ]*\).*/\1/p" "$REPO_ROOT/scripts/render-brand.sh")"; do
      dk rmi "$i" >/dev/null 2>&1 || true
    done
    nas mkdir -p "$NAS_SCRATCH"
    dk run --rm --platform "$PLATFORM" -v "$NAS_SCRATCH:/ws" "$NAS_ALPINE_IMAGE" \
      find /ws -mindepth 1 -delete >/dev/null
    # rmdir only ever removes an empty directory, so this can't reach anything else in workspace/.
    dk run --rm --platform "$PLATFORM" -v "$NAS_HOME/workspace:/w" "$NAS_ALPINE_IMAGE" \
      rmdir /w/reses-ci >/dev/null
    dk rmi "$NAS_ALPINE_IMAGE" >/dev/null 2>&1 || true
    echo "removed every reses container, network, volume, image and scratch directory from $NAS" ;;
  *) echo "unknown option: $1" >&2; exit 2 ;;
esac
