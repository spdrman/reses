#!/usr/bin/env bash
# Run reses builds and tests inside the CI image. Nothing here touches the host toolchain.
#
#   scripts/ci-docker.sh              full gate: fmt, clippy, tests, python oracle tests
#   scripts/ci-docker.sh --exec CMD   run CMD in the container (e.g. "cargo test mail")
#   scripts/ci-docker.sh --shell      interactive shell
#   scripts/ci-docker.sh --darwin     build the macOS release binary into dist/
#   scripts/ci-docker.sh --integration  run the MinIO-backed S3 tests
#
# RESES_LANE names the cargo target volume, so parallel worktrees never share build output.
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
LANE="${RESES_LANE:-main}"
IMAGE="reses-ci:1"
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
cargo test --locked --no-fail-fast
(cd python && python3 -m unittest -q)'

case "${1:-}" in
  "") run "$GATE" ;;
  --exec) shift; run "$*" ;;
  --shell) run bash ;;
  --darwin)
    run 'set -e
cargo zigbuild --release --locked --target aarch64-apple-darwin
mkdir -p dist && cp /target/aarch64-apple-darwin/release/reses dist/reses-aarch64-apple-darwin
ls -l dist/' ;;
  --integration)
    NET="reses-it-${LANE}"
    MINIO="reses-minio-${LANE}"
    docker network create "$NET" >/dev/null 2>&1 || true
    docker rm -f "$MINIO" >/dev/null 2>&1 || true
    docker run -d --rm --platform "$PLATFORM" --name "$MINIO" --network "$NET" \
      -e MINIO_ROOT_USER=resesadmin -e MINIO_ROOT_PASSWORD=resesadmin-secret \
      minio/minio:RELEASE.2025-04-22T22-12-26Z server /data >/dev/null
    trap 'docker rm -f "$MINIO" >/dev/null 2>&1 || true; docker network rm "$NET" >/dev/null 2>&1 || true' EXIT
    EXTRA_DOCKER_ARGS=(--network "$NET"
      -e RESES_TEST_S3_ENDPOINT="http://$MINIO:9000"
      -e RESES_TEST_S3_ACCESS_KEY=resesadmin -e RESES_TEST_S3_SECRET_KEY=resesadmin-secret)
    run 'cargo test --locked --no-fail-fast -- --ignored --test-threads=1' ;;
  *) echo "unknown option: $1" >&2; exit 2 ;;
esac
