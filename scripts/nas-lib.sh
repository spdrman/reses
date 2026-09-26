# Shared helpers for running reses's Docker work on the UGREEN NAS. Sourced, not run, by
# scripts/ci-docker.sh, demo/record.sh and scripts/render-brand.sh.
#
# I keep every rule the owner set for that machine in one place, so no script can drift from
# them: the NAS itself only ever runs ssh, docker, and tar into one scratch directory,
# ~/workspace/reses-ci/; every job runs in a --rm container; anything the containers wrote as
# root is removed from inside a container; and no token or key is ever sent over.
#
# After sourcing, NAS_HOME, NAS_SCRATCH and NAS_PLATFORM are set, and these are available:
#   nas CMD...                 run one command on the NAS, each argument quoted exactly
#   dk ARGS...                 run docker on the NAS
#   nas_push LANE              send this worktree to $NAS_SCRATCH/LANE, fresh
#   nas_scrub PATH_UNDER_SCRATCH   remove a scratch path from inside a container
#   nas_fetch REMOTE_DIR LOCAL_DIR PATH...   copy files back from the NAS

NAS="${RESES_NAS:-rom@192.168.0.10}"
NAS_PLATFORM="linux/amd64"
# The third-party images reses runs, pinned by digest: MinIO RELEASE.2026-09-22T19-25-18Z for the
# integration suite and the demo, and vhs v0.12.1 for recording it.
NAS_MINIO_IMAGE="cgr.dev/chainguard/minio@sha256:bd014394a80898e68c149f2311fdf8d5a2c2f3bb2c33b9327ae6d02b4b065ae1"
NAS_VHS_IMAGE="ghcr.io/charmbracelet/vhs@sha256:ea49a6a1c529be83153e88321892b5585964418f0b9055e8c1e0d732194234a3"
NAS_SSH=(ssh -o BatchMode=yes -o ConnectTimeout=10 -o LogLevel=ERROR "$NAS")

# I quote each argument with printf %q, so the NAS's shell sees exactly what the caller passed.
nas() { "${NAS_SSH[@]}" "$(printf '%q ' "$@")"; }
dk() { nas docker "$@"; }

# Asked once, and a failure here is the one place a missing NAS gets a readable message.
if ! NAS_HOME="$("${NAS_SSH[@]}" 'printf %s "$HOME"' 2>/dev/null)" || [ -z "$NAS_HOME" ]; then
  echo "can't reach the build host $NAS over ssh. Check the network and your ssh key, or point" >&2
  echo "RESES_NAS at another Docker host you can ssh to (user@host)." >&2
  exit 1
fi
NAS_SCRATCH="$NAS_HOME/workspace/reses-ci"

# A lane name becomes a directory name and a volume name, so it has to be plain.
nas_check_lane() {
  [[ "$1" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ && "$1" != *..* ]] \
    || { echo "not a usable lane name: '$1' (letters, digits, dot, dash, underscore)" >&2; exit 2; }
}

# Two runs in one lane would push over each other's tree mid-build and fail believably, so a
# lane is held by a lock directory beside it on the NAS. mkdir is atomic, so only one run gets it.
# A script that holds the lane exports that, and the scripts it calls reuse the hold.
nas_lock() {
  local lane="$1"
  nas_check_lane "$lane"
  [ "${RESES_LANE_HELD:-}" = "$lane" ] && return 0
  nas mkdir -p "$NAS_SCRATCH"
  if ! nas mkdir "$NAS_SCRATCH/.lock-$lane" 2>/dev/null; then
    echo "lane '$lane' is in use on $NAS. Pick another RESES_LANE, or if no run is going there," >&2
    echo "clear the stale lock with: scripts/ci-docker.sh --nas-unlock $lane" >&2
    exit 1
  fi
  export RESES_LANE_HELD="$lane"
  NAS_LOCK_MINE="$lane"
}

# Only the run that took the lock gives it back.
nas_unlock() {
  [ -n "${NAS_LOCK_MINE:-}" ] || return 0
  nas_scrub ".lock-$NAS_LOCK_MINE" || true
  NAS_LOCK_MINE=
}

# The containers write as root, so the host account can't delete what they leave. I remove it
# from inside a container instead. Only reses-ci/ is mounted, so nothing outside it is reachable,
# and the path goes to rm as an argument, never pasted into shell code.
nas_scrub() {
  case "$1" in "" | *..* | /* | -*) echo "nas_scrub: refusing '$1'" >&2; return 1 ;; esac
  nas mkdir -p "$NAS_SCRATCH"
  dk run --rm --platform "$NAS_PLATFORM" -v "$NAS_SCRATCH:/ws" alpine:3 \
    rm -rf -- "/ws/$1" >/dev/null 2>&1 || true
}

# I send the files git would see (tracked and untracked, not ignored), skip tracked files that
# are deleted here, and leave their timestamps at "now" (tar -m) so cargo always rebuilds what
# changed rather than trusting an older build. Untracked files are listed as they go, and any
# that look like a secret or real mail stop the push, so nothing like that ever lands on the NAS.
nas_push() {
  local lane="$1" root risky
  nas_check_lane "$lane"
  nas_lock "$lane"
  root="$(git rev-parse --show-toplevel)"
  risky=$(cd "$root" && git ls-files -o --exclude-standard \
    | grep -iE '(^|/)(\.npmrc|\.netrc|id_[a-z0-9]+|credentials[^/]*)$|\.(pem|key|p12|pfx)$' \
    | grep -vE '^tests/fixtures/' || true)
  risky+=$(cd "$root" && git ls-files -o --exclude-standard | grep -E '\.eml$' | grep -vE '^tests/fixtures/' || true)
  if [ -n "$risky" ]; then
    echo "refusing to send untracked files that look like secrets or real mail:" >&2
    printf '  %s\n' $risky >&2
    exit 1
  fi
  (cd "$root" && git ls-files -o --exclude-standard | sed 's/^/  sending untracked: /' >&2) || true
  nas_scrub "$lane"
  # A scrub that silently failed would leave the previous run's files in the tree.
  if [ -n "$(nas sh -c "ls -A '$NAS_SCRATCH/$lane' 2>/dev/null")" ]; then
    echo "couldn't clear $NAS_SCRATCH/$lane on the NAS, so I'm not running on a stale tree" >&2
    exit 1
  fi
  nas mkdir -p "$NAS_SCRATCH/$lane"
  (cd "$root" && git ls-files -z -co --exclude-standard \
    | perl -0ne 'chomp; print "$_\0" if -e $_' \
    | tar --null -T - -cf -) | nas tar -xmf - -C "$NAS_SCRATCH/$lane"
  # The tracked list goes too, so the container can index exactly what git tracks here.
  (cd "$root" && git ls-files -z -c | perl -0ne 'chomp; print "$_\0" if -e $_') \
    | nas sh -c "cat > '$NAS_SCRATCH/$lane/.reses-tracked'"
}

# Copy files back from a NAS directory into a local one.
nas_fetch() {
  local from="$1" to="$2"
  shift 2
  mkdir -p "$to"
  nas tar -cf - -C "$from" "$@" | tar -xf - -C "$to"
}
