# Shared helpers for running reses's Docker work on the UGREEN NAS. Sourced, not run, by
# scripts/ci-docker.sh, demo/record.sh and scripts/render-brand.sh.
#
# I keep every rule the owner set for that machine in one place, so no script can drift from
# them: the NAS itself only ever runs ssh, docker, and small reads and writes (tar, mkdir, cat,
# ls) in one scratch directory, ~/workspace/reses-ci/; every job runs in a --rm container;
# anything the containers wrote as root is removed from inside a container; and no token or key
# is ever sent over.
#
# After sourcing, NAS_HOME, NAS_SCRATCH and NAS_PLATFORM are set, and these are available:
#   nas CMD...                 run one command on the NAS, each argument quoted exactly
#   dk ARGS...                 run docker on the NAS
#   nas_lock LANE              take a lane, or stop if another run holds it
#   nas_unlock                 give back the lane this run took
#   nas_lock_still_mine        true while this run still holds its lane
#   nas_push LANE              take the lane and send this worktree to $NAS_SCRATCH/LANE, fresh
#   nas_scrub PATH_UNDER_SCRATCH   remove a scratch path from inside a container
#   nas_fetch REMOTE_DIR LOCAL_DIR PATH...   copy files back from the NAS

NAS="${RESES_NAS:-rom@192.168.0.10}"
NAS_PLATFORM="linux/amd64"
# The third-party images reses runs, pinned by digest: MinIO RELEASE.2026-09-22T19-25-18Z for the
# integration suite and the demo, and vhs v0.12.1 for recording it.
NAS_MINIO_IMAGE="cgr.dev/chainguard/minio@sha256:bd014394a80898e68c149f2311fdf8d5a2c2f3bb2c33b9327ae6d02b4b065ae1"
NAS_VHS_IMAGE="ghcr.io/charmbracelet/vhs@sha256:ea49a6a1c529be83153e88321892b5585964418f0b9055e8c1e0d732194234a3"
# alpine runs the scrubs, pinned like the rest (the digest alpine:3 had on the NAS on 2026-09-26).
NAS_ALPINE_IMAGE="alpine@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6"
NAS_SSH=(ssh -o BatchMode=yes -o ConnectTimeout=10 -o LogLevel=ERROR "$NAS")

# I quote each argument with printf %q, so the NAS's shell sees exactly what the caller passed.
# bash's %q writes newlines as $'...', so the NAS login shell has to be bash or zsh.
nas() { "${NAS_SSH[@]}" "$(printf '%q ' "$@")"; }
# docker on the NAS, through the same quoting.
dk() { nas docker "$@"; }

# Asked once, and a failure here is the one place a missing NAS gets a readable message.
if ! NAS_HOME="$("${NAS_SSH[@]}" 'printf %s "$HOME"' 2>&1)" || [ -z "$NAS_HOME" ] || [[ "$NAS_HOME" != /* ]]; then
  echo "can't reach the build host $NAS over ssh:" >&2
  printf '  %s\n' "${NAS_HOME:-(no output)}" >&2
  echo "Check the network, your ssh key and known_hosts (the first connection has to be made by hand), or point" >&2
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
# lane is held by a lock directory beside it on the NAS. mkdir is atomic, so only one run gets it,
# and it writes an owner line inside. A run only clears the lane or gives the lock back while that
# line is still its own, so a run that stalls, loses its lock to --nas-unlock and then wakes up
# can't wipe the tree of whoever holds the lane now. A script that holds the lane exports that,
# and the scripts it calls reuse the hold.
nas_lock() {
  local lane="$1" lock
  nas_check_lane "$lane"
  [ "${RESES_LANE_HELD:-}" = "$lane" ] && return 0
  lock="$NAS_SCRATCH/.lock-$lane"
  nas mkdir -p "$NAS_SCRATCH"
  if ! nas mkdir "$lock" 2>/dev/null; then
    if [ -z "$(nas_lock_owner "$lane")" ] && ! nas test -d "$lock"; then
      echo "couldn't create the lock for lane '$lane' in $NAS_SCRATCH on $NAS (permissions or disk?)" >&2
      exit 1
    fi
    echo "lane '$lane' is in use on $NAS, by: $(nas_lock_owner "$lane" || true)" >&2
    echo "Pick another RESES_LANE, or if that run is gone, clear it with: scripts/ci-docker.sh --nas-unlock $lane" >&2
    exit 1
  fi
  NAS_LOCK_TOKEN="$(hostname -s 2>/dev/null || echo host) pid $$ since $(date -u +%Y-%m-%dT%H:%M:%SZ) #$RANDOM$RANDOM"
  printf '%s\n' "$NAS_LOCK_TOKEN" | nas sh -c "cat > '$lock/owner'"
  export RESES_LANE_HELD="$lane"
  NAS_LOCK_MINE="$lane"
}

# The owner line of a lane's lock, read from inside a container, or nothing if there's no lock.
nas_lock_owner() {
  dk run --rm --platform "$NAS_PLATFORM" -v "$NAS_SCRATCH:/ws:ro" "$NAS_ALPINE_IMAGE" \
    cat "/ws/.lock-$1/owner" 2>/dev/null </dev/null || true
}

# True while this run took the lane's lock and nobody has taken it since.
nas_lock_still_mine() {
  [ -n "${NAS_LOCK_MINE:-}" ] && [ "$(nas_lock_owner "$NAS_LOCK_MINE")" = "$NAS_LOCK_TOKEN" ]
}

# Only the run that holds the lock gives it back, and only while it's still its own.
nas_unlock() {
  if nas_lock_still_mine; then nas_scrub ".lock-$NAS_LOCK_MINE" || true; fi
  NAS_LOCK_MINE=
}

# The containers write as root, so the host account can't delete what they leave. I remove it
# from inside a container instead. Only reses-ci/ is mounted, so nothing outside it is reachable,
# and the path goes to rm as an argument, never pasted into shell code.
nas_scrub() {
  case "$1" in "" | *..* | /* | -*) echo "nas_scrub: refusing '$1'" >&2; return 1 ;; esac
  nas mkdir -p "$NAS_SCRATCH"
  if ! dk run --rm --platform "$NAS_PLATFORM" -v "$NAS_SCRATCH:/ws" "$NAS_ALPINE_IMAGE" \
    rm -rf -- "/ws/$1" </dev/null >/dev/null 2>&1; then
    echo "warning: couldn't remove $NAS_SCRATCH/$1 on $NAS" >&2
    return 1
  fi
}

# Untracked files that must never leave this machine: key and credential files by name, and
# anything whose first lines look like a stored mail message. Raw SES mail from S3 has no
# extension, so I look at the headers rather than the name. Fixtures under tests/fixtures/ are
# fake mail on purpose and are allowed.
nas_risky_untracked() {
  local root="$1"
  (cd "$root" && git ls-files -z -o --exclude-standard) | perl -0ne '
    chomp;
    next if m{^tests/fixtures/};
    my $bad = m{(^|/)(\.npmrc|\.netrc|\.git-credentials|\.pgpass|id_[a-z0-9]+|credentials[^/]*)$}i
      || m{\.(pem|key|p12|pfx|eml|mbox)$}i;
    if (!$bad && -f $_ && open(my $fh, "<", $_)) {
      read($fh, my $head, 8192);
      $bad = $head =~ m{^(Received|Return-Path|DKIM-Signature|X-SES-[A-Za-z-]+):}mi;
    }
    print "$_\n" if $bad;'
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
  risky="$(nas_risky_untracked "$root")"
  if [ -n "$risky" ]; then
    echo "refusing to send untracked files that look like secrets or real mail:" >&2
    printf '%s\n' "$risky" | sed 's/^/  /' >&2
    exit 1
  fi
  (cd "$root" && git ls-files -o --exclude-standard | sed 's/^/  sending untracked: /' >&2) || true
  nas_scrub "$lane" || true
  # A scrub that silently failed would leave the previous run's files in the tree.
  if [ -n "$(nas sh -c "ls -A '$NAS_SCRATCH/$lane' 2>/dev/null")" ]; then
    echo "couldn't clear $NAS_SCRATCH/$lane on the NAS, so I'm not running on a stale tree" >&2
    exit 1
  fi
  nas mkdir -p "$NAS_SCRATCH/$lane"
  # COPYFILE_DISABLE keeps macOS tar from adding ._ files for extended attributes.
  (cd "$root" && git ls-files -z -co --exclude-standard \
    | perl -0ne 'chomp; print "$_\0" if -e $_' \
    | COPYFILE_DISABLE=1 tar --null -T - -cf -) | nas tar -xmf - -C "$NAS_SCRATCH/$lane"
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
