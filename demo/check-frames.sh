#!/usr/bin/env bash
# Check the text frames vhs wrote next to the demo GIF, so a recording of a broken screen
# can never replace docs/demo.gif. record.sh runs this in the CI image.
#
#   demo/check-frames.sh demo.txt
#
# vhs writes one screen per frame, separated by a line of box-drawing dashes. Each step below
# has to appear in its own frame, in the order the tape plays them, and the last frame has to
# be the inbox again. A pattern starting with ! must not match that frame.
set -euo pipefail

txt=${1:?usage: check-frames.sh FRAMES.txt}
[ -s "$txt" ] || { echo "FAIL: $txt is missing or empty" >&2; exit 1; }

dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
awk -v d="$dir" '
  index($0, "────────────────────") == 1 { n++; next }
  { f = sprintf("%s/%05d", d, n); print >> f; close(f) }
' "$txt"
frames=("$dir"/*)
last=$((${#frames[@]} - 1))
echo "${#frames[@]} frames in $txt"

failed=0

# Does frame $1 match every pattern after it?
matches() {
  local f=$1 pat
  shift
  for pat in "$@"; do
    if [ "${pat:0:1}" = "!" ]; then
      grep -Eq -- "${pat:1}" "$f" && return 1
    else
      grep -Eq -- "$pat" "$f" || return 1
    fi
  done
  return 0
}

# step NAME PATTERN...: the first frame after the previous step's that matches them all.
at=-1
step() {
  local name=$1 i
  shift
  for ((i = at + 1; i <= last; i++)); do
    if matches "${frames[$i]}" "$@"; then
      echo "ok    $name (frame $i)"
      at=$i
      return
    fi
  done
  echo "FAIL  $name: no frame after $at shows it" >&2
  failed=1
}

step "the accounts screen" \
  'reses +Accounts' \
  'AWS profiles in /home/demo/\.aws/credentials' \
  '> default +us-east-1'

step "the bucket list" \
  'Browse S3 +default \(us-east-1\)' \
  '3 buckets' \
  '> mail-inbound'

step "the folder with its email marks" \
  'mail-inbound/inbound/' \
  '12 objects · 10 emails' \
  '[0-9a-z]{40} +[0-9.]+ (B|KiB) +email *$' \
  'export-2026-09\.csv +32 B *$'

step "the saved inbox, loaded" \
  'saved mail-inbound/inbound/ as the inbox' \
  'Inbox s3://mail-inbound/inbound/ · 10 messages · 2 not email' \
  '^ *From +Subject +Date +Size *$' \
  'Sam Ortiz +Welcome aboard! Notes for your first week' \
  'Mei Chen +Draft agenda for Thursday.s planning call .* 1\.7 KiB *$' \
  'The Weekly Tinkerer +Issue 118: a soldering station roundup' \
  '!loading…'

step "the opened message" \
  'Message s3://mail-inbound/inbound/' \
  '^From: Mei Chen <mei\.chen@example\.com>' \
  '^Subject: Draft agenda for Thursday.s planning call' \
  '^Hi all,'

step "the message, scrolled" \
  'Message s3://mail-inbound/inbound/' \
  '!^Reply-To:' \
  'Desks go in over the weekend, so pack up by Friday' \
  '^Cheers,'

step "back in the inbox" \
  '^ *From +Subject +Date +Size *$' \
  'Mei Chen +Draft agenda for Thursday.s planning call' \
  '!Message s3://'

if [ "$at" -ne "$last" ] && ! matches "${frames[$last]}" '^ *From +Subject +Date +Size *$' 'Mei Chen +Draft agenda' '!Message s3://'; then
  echo "FAIL  the last frame is not the inbox" >&2
  failed=1
else
  echo "ok    the last frame is the inbox (frame $last)"
fi

if bad=$(grep -EHn 'panicked|[Ee]rror|could not|denied|[Nn]ot connected|failed|does not exist' "${frames[@]}"); then
  echo "FAIL  error text on screen:" >&2
  echo "$bad" | sed "s|$dir/||" >&2
  failed=1
else
  echo "ok    no error text in any frame"
fi

if [ "$failed" -ne 0 ]; then
  echo "the recording does not show the demo; docs/demo.gif is unchanged" >&2
  exit 1
fi
echo "all frame checks passed"
