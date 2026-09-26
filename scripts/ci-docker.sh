#!/usr/bin/env bash
# Run reses builds and tests inside the CI image. Nothing here touches the host toolchain.
#
#   scripts/ci-docker.sh              full gate: the Check, MSRV, Docs and Test jobs of ci.yml
#   scripts/ci-docker.sh --exec CMD   run CMD in the container (e.g. "cargo test mail")
#   scripts/ci-docker.sh --shell      interactive shell
#   scripts/ci-docker.sh --darwin     build the macOS release binary into dist/
#   scripts/ci-docker.sh --integration  run the MinIO-backed S3 tests
#   scripts/ci-docker.sh --image-tag  print the CI image tag (demo/record.sh uses it)
#
# RESES_LANE names the cargo target volume, so parallel worktrees never share build output.
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
LANE="${RESES_LANE:-main}"
# Tagged by a hash of the Dockerfile, so editing the Dockerfile builds a fresh image
# instead of silently reusing the old one.
IMAGE="reses-ci:$(shasum -a 256 "$REPO_ROOT/docker/ci.Dockerfile" | cut -c1-12)"

if [ "${1:-}" = "--image-tag" ]; then
  echo "$IMAGE"
  exit 0
fi

PLATFORM="linux/arm64"
TARGET_VOL="reses-target-${LANE}"
REGISTRY_VOL="reses-cargo-registry"

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  docker build --platform "$PLATFORM" -t "$IMAGE" -f "$REPO_ROOT/docker/ci.Dockerfile" "$REPO_ROOT/docker"
fi

run() {
  local tty=()
  [ -t 0 ] && [ -t 1 ] && tty=(-it)
  docker run --rm ${tty[@]+"${tty[@]}"} --platform "$PLATFORM" \
    --cpus "${RESES_CPUS:-2}" --memory "${RESES_MEMORY:-3g}" \
    -e CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-auto}" -e CARGO_BUILD_JOBS="${RESES_CPUS:-2}" -e CARGO_TARGET_DIR=/target \
    ${EXTRA_DOCKER_ARGS[@]+"${EXTRA_DOCKER_ARGS[@]}"} \
    -v "$REGISTRY_VOL:/usr/local/cargo/registry" \
    -v "$TARGET_VOL:/target" \
    -v "$REPO_ROOT:$REPO_ROOT" -w "$REPO_ROOT" \
    "$IMAGE" bash -c "$1"
}
EXTRA_DOCKER_ARGS=()

GATE='set -e
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo check --all-targets --locked
RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links -D rustdoc::redundant_explicit_links" cargo doc --no-deps --locked
cargo test --locked --no-fail-fast'

case "${1:-}" in
  "") run "$GATE" ;;
  --exec) shift; run "$*" ;;
  --shell) run bash ;;
  --darwin)
    # place-binary.sh rather than cp: the host runs the result, and a cp over the previous
    # build leaves a binary macOS kills at exec (#15).
    run 'set -e
cargo zigbuild --release --locked --target aarch64-apple-darwin
mkdir -p dist && scripts/place-binary.sh /target/aarch64-apple-darwin/release/reses dist/reses-aarch64-apple-darwin
ls -l dist/' ;;
  --integration)
    NET="reses-it-${LANE}"
    MINIO="reses-minio-${LANE}"
    # minio/minio is no longer pullable from Docker Hub, so I use Chainguard's build, pinned
    # by digest (MinIO RELEASE.2026-09-22T19-25-18Z). It runs as a non-root user, so the
    # data directory lives under /tmp.
    MINIO_IMAGE="cgr.dev/chainguard/minio@sha256:bd014394a80898e68c149f2311fdf8d5a2c2f3bb2c33b9327ae6d02b4b065ae1"
    # Clean up first thing, so a failed start never leaves the network or container behind.
    trap 'docker rm -f "$MINIO" >/dev/null 2>&1 || true; docker network rm "$NET" >/dev/null 2>&1 || true' EXIT
    docker network create "$NET" >/dev/null 2>&1 || true
    docker rm -f "$MINIO" >/dev/null 2>&1 || true
    docker run -d --rm --platform "$PLATFORM" --name "$MINIO" --network "$NET" \
      -e MINIO_ROOT_USER=resesadmin -e MINIO_ROOT_PASSWORD=resesadmin-secret \
      "$MINIO_IMAGE" server /tmp/minio-data >/dev/null
    EXTRA_DOCKER_ARGS=(--network "$NET"
      -e RESES_TEST_S3_ENDPOINT="http://$MINIO:9000"
      -e RESES_TEST_S3_ACCESS_KEY=resesadmin -e RESES_TEST_S3_SECRET_KEY=resesadmin-secret)
    # MinIO takes a moment to listen; wait for its health check rather than racing it.
    run 'set -e
for i in $(seq 1 60); do
  curl -fs -o /dev/null "$RESES_TEST_S3_ENDPOINT/minio/health/live" && break
  [ "$i" = 60 ] && { echo "MinIO never came up" >&2; exit 1; }
  sleep 1
done
cargo test --locked --no-fail-fast -- --ignored --test-threads=1' ;;
  *) echo "unknown option: $1" >&2; exit 2 ;;
esac
