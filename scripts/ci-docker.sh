#!/usr/bin/env bash
# Run reses builds and tests in the CI image on the UGREEN NAS. Nothing here touches this Mac's
# toolchain or its Docker.
#
#   scripts/ci-docker.sh                full gate: the Check, MSRV, Docs and Test jobs of ci.yml
#   scripts/ci-docker.sh --exec CMD     run CMD in the container (e.g. "cargo test mail")
#   scripts/ci-docker.sh --shell        interactive shell
#   scripts/ci-docker.sh --darwin       build the macOS release binary into dist/ on this Mac
#   scripts/ci-docker.sh --integration  run the MinIO-backed S3 tests
#   scripts/ci-docker.sh --image-tag    print the CI image tag (demo/record.sh uses it)
#   scripts/ci-docker.sh --nas-clean    remove every reses container, image, volume and scratch
#                                       directory from the NAS
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

NAS="${RESES_NAS:-rom@192.168.0.10}"
SSH=(ssh -o BatchMode=yes -o LogLevel=ERROR "$NAS")
PLATFORM="linux/amd64"
TARGET_VOL="reses-target-${LANE}"
REGISTRY_VOL="reses-cargo-registry"

# Run a command on the NAS, each argument quoted so the remote shell sees it exactly.
nas() { "${SSH[@]}" "$(printf '%q ' "$@")"; }
# Run docker on the NAS.
dk() { nas docker "$@"; }

NAS_HOME="$("${SSH[@]}" 'printf %s "$HOME"')"
SCRATCH="$NAS_HOME/workspace/reses-ci"
W="$SCRATCH/$LANE"

# Remove a lane's scratch tree from inside a container. The containers write as root, so the
# host account can't delete what they leave, and deleting on the host is what this avoids.
scrub() {
  dk run --rm --platform "$PLATFORM" -v "$NAS_HOME/workspace:/ws" alpine:3 \
    sh -c "rm -rf '/ws/reses-ci/$1'" >/dev/null 2>&1 || true
}

# Send this worktree to the NAS: the files git would see (tracked and untracked, not ignored),
# skipping any tracked file that's been deleted here. `tar -m` leaves timestamps at "now".
push() {
  scrub "$LANE"
  nas mkdir -p "$W"
  (cd "$REPO_ROOT" && git ls-files -z -co --exclude-standard \
    | perl -0ne 'chomp; print "$_\0" if -e $_' \
    | tar --null -T - -cf -) | nas tar -xmf - -C "$W"
}

if ! dk image inspect "$IMAGE" >/dev/null 2>&1; then
  "${SSH[@]}" "docker build --platform $PLATFORM -t $IMAGE -" < "$REPO_ROOT/docker/ci.Dockerfile"
fi

# Run a command in the CI image with the pushed tree mounted at its own path.
run() {
  local tty=()
  [ "${INTERACTIVE:-}" = 1 ] && tty=(-it)
  local cmd='[ -d .git ] || { git init -q && git add -A >/dev/null 2>&1; }
'"$1"
  local args=(docker run --rm ${tty[@]+"${tty[@]}"} --platform "$PLATFORM"
    --cpus "${RESES_CPUS:-2}" --memory "${RESES_MEMORY:-3g}"
    -e CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-auto}" -e CARGO_BUILD_JOBS="${RESES_CPUS:-2}"
    -e CARGO_TARGET_DIR=/target
    -e GIT_CONFIG_COUNT=1 -e GIT_CONFIG_KEY_0=safe.directory -e GIT_CONFIG_VALUE_0='*'
    ${EXTRA_DOCKER_ARGS[@]+"${EXTRA_DOCKER_ARGS[@]}"}
    -v "$REGISTRY_VOL:/usr/local/cargo/registry"
    -v "$TARGET_VOL:/target"
    -v "$W:$W" -w "$W"
    "$IMAGE" bash -c "$cmd")
  if [ ${#tty[@]} -gt 0 ]; then
    ssh -t -o BatchMode=yes -o LogLevel=ERROR "$NAS" "$(printf '%q ' "${args[@]}")"
  else
    nas "${args[@]}"
  fi
}
EXTRA_DOCKER_ARGS=()

GATE='set -e
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo check --all-targets --locked
RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links -D rustdoc::redundant_explicit_links" cargo doc --no-deps --locked
cargo test --locked --no-fail-fast'

case "${1:-}" in
  "") push; run "$GATE" ;;
  --exec) shift; push; run "$*" ;;
  --shell) push; INTERACTIVE=1 run bash ;;
  --darwin)
    push
    # place-binary.sh rather than cp, both on the NAS and back here: the Mac runs the result,
    # and a cp over the previous build leaves a binary macOS kills at exec (#15).
    run 'set -e
cargo zigbuild --release --locked --target aarch64-apple-darwin
mkdir -p dist && scripts/place-binary.sh /target/aarch64-apple-darwin/release/reses dist/reses-aarch64-apple-darwin
ls -l dist/'
    back="$(mktemp -d)"
    trap 'rm -rf "$back"' EXIT
    nas tar -cf - -C "$W" dist/reses-aarch64-apple-darwin | tar -xf - -C "$back"
    mkdir -p "$REPO_ROOT/dist"
    "$REPO_ROOT/scripts/place-binary.sh" "$back/dist/reses-aarch64-apple-darwin" "$REPO_ROOT/dist/reses-aarch64-apple-darwin"
    ls -l "$REPO_ROOT/dist/" ;;
  --integration)
    NET="reses-it-${LANE}"
    MINIO="reses-minio-${LANE}"
    # minio/minio is no longer pullable from Docker Hub, so I use Chainguard's build, pinned
    # by digest (MinIO RELEASE.2026-09-22T19-25-18Z). It runs as a non-root user, so the
    # data directory lives under /tmp.
    MINIO_IMAGE="cgr.dev/chainguard/minio@sha256:bd014394a80898e68c149f2311fdf8d5a2c2f3bb2c33b9327ae6d02b4b065ae1"
    # Clean up first thing, so a failed start never leaves the network or container behind.
    trap 'dk rm -f "$MINIO" >/dev/null 2>&1 || true; dk network rm "$NET" >/dev/null 2>&1 || true' EXIT
    dk network create "$NET" >/dev/null 2>&1 || true
    dk rm -f "$MINIO" >/dev/null 2>&1 || true
    dk run -d --rm --platform "$PLATFORM" --name "$MINIO" --network "$NET" \
      -e MINIO_ROOT_USER=resesadmin -e MINIO_ROOT_PASSWORD=resesadmin-secret \
      "$MINIO_IMAGE" server /tmp/minio-data >/dev/null
    EXTRA_DOCKER_ARGS=(--network "$NET"
      -e RESES_TEST_S3_ENDPOINT="http://$MINIO:9000"
      -e RESES_TEST_S3_ACCESS_KEY=resesadmin -e RESES_TEST_S3_SECRET_KEY=resesadmin-secret)
    push
    # MinIO takes a moment to listen; wait for its health check rather than racing it.
    run 'set -e
for i in $(seq 1 60); do
  curl -fs -o /dev/null "$RESES_TEST_S3_ENDPOINT/minio/health/live" && break
  [ "$i" = 60 ] && { echo "MinIO never came up" >&2; exit 1; }
  sleep 1
done
cargo test --locked --no-fail-fast -- --ignored --test-threads=1' ;;
  --nas-clean)
    # Everything reses put on the NAS, and nothing that belongs to anyone else.
    for c in $(dk ps -aq --filter name=reses-); do dk rm -f "$c" >/dev/null; done
    for n in $(dk network ls -q --filter name=reses-); do dk network rm "$n" >/dev/null; done
    for v in $(dk volume ls -q --filter name=reses-); do dk volume rm "$v" >/dev/null; done
    for i in $(dk image ls -q --filter reference='reses-*'); do dk rmi -f "$i" >/dev/null; done
    dk run --rm --platform "$PLATFORM" -v "$NAS_HOME/workspace:/ws" alpine:3 sh -c 'rm -rf /ws/reses-ci' >/dev/null
    echo "removed every reses container, network, volume, image and scratch directory from $NAS" ;;
  *) echo "unknown option: $1" >&2; exit 2 ;;
esac
