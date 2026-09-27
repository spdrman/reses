#!/usr/bin/env bash
# Check the text snapshots vhs wrote next to the demo GIF, so a recording of a broken screen
# can never replace docs/demo.gif. record.sh runs this in the CI image, and tests/demo_frames.rs
# runs it on the fixtures in tests/fixtures/demo/.
#
#   demo/check-frames.sh FRAMES.txt MESSAGES.tsv
#
# vhs writes one snapshot per tape command, separated by a line of box-drawing dashes. These are
# the checked snapshots: the GIF frames between them are not. Each step below has to appear in
# its own snapshot, in the order the tape plays them, and the last one has to be the inbox again.
# Every message in MESSAGES.tsv (demo/messages.tsv, which seed.sh uploads) has to be a row of
# the loaded inbox. A pattern starting with ! must not match that snapshot.
set -euo pipefail

txt=${1:?usage: check-frames.sh FRAMES.txt MESSAGES.tsv}
messages=${2:?usage: check-frames.sh FRAMES.txt MESSAGES.tsv}
[ -s "$txt" ] || { echo "FAIL  $txt is missing or empty" >&2; exit 1; }
[ -s "$messages" ] || { echo "FAIL  $messages is missing or empty" >&2; exit 1; }

dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
mkdir "$dir/frames"
# Append, not >: after close() mawk truncates a file that print > opens again.
awk -v d="$dir/frames" '
  index($0, "────────────────────") == 1 { n++; next }
  { f = sprintf("%s/%05d", d, n); print >> f; close(f) }
' "$txt"
frames=("$dir"/frames/*)
last=$((${#frames[@]} - 1))
echo "${#frames[@]} snapshots in $txt"

failed=0

# grep -E, but a status of 2 (a bad pattern, an unreadable file) stops the check rather than
# reading as "no match".
grepq() {
  local s=0
  grep "$@" >/dev/null || s=$?
  if [ "$s" -gt 1 ]; then
    echo "FAIL  grep $* exited $s" >&2
    exit 2
  fi
  return "$s"
}

# Does snapshot $1 match every pattern after it?
matches() {
  local f=$1 pat
  shift
  for pat in "$@"; do
    if [ "${pat:0:1}" = "!" ]; then
      grepq -Eq -- "${pat:1}" "$f" && return 1
    else
      grepq -Eq -- "$pat" "$f" || return 1
    fi
  done
  return 0
}

# step NAME PATTERN...: the first snapshot after the previous step's that matches them all.
at=-1
step() {
  local name=$1 i
  shift
  for ((i = at + 1; i <= last; i++)); do
    if matches "${frames[$i]}" "$@"; then
      echo "ok    $name (snapshot $i)"
      at=$i
      return
    fi
  done
  echo "FAIL  $name: no snapshot after $at shows it" >&2
  failed=1
}

# A literal string as an extended regex.
literal() {
  printf '%s' "$1" | sed 's/[][\.*^$+?(){}|/]/\\&/g'
}

# One inbox row per message: sender name, subject, a date (HH:MM today, else "Sep 25") and a size.
rows=()
count=0
while IFS=$'\t' read -r id age name address subject; do
  case $id in '#'* | '') continue ;; esac
  rows+=("^ *$(literal "$name") +$(literal "$subject") +([0-9]{2}:[0-9]{2}|[A-Z][a-z]{2} [0-9]{1,2}) +[0-9.]+ (B|KiB|MiB) *$")
  count=$((count + 1))
done <"$messages"
[ "$count" -gt 0 ] || { echo "FAIL  no messages in $messages" >&2; exit 1; }

header='^ *From +Subject +Date +Size *$'
# The header bar opens with the logo. vhs records in xterm.js, which gets the styled-text
# wordmark: three squares stacked like the cubes (▄▀▄), then "re:SES", then the screen's title.
logo='^ ▄▀▄ re:SES  '

step "the accounts screen" \
  "${logo}Accounts" \
  'AWS profiles in /home/demo/\.aws/credentials' \
  '> default +us-east-1'

step "the bucket list" \
  "${logo}Browse S3 +default \\(us-east-1\\)" \
  '3 buckets' \
  '> mail-inbound'

step "the folder with its email marks" \
  'mail-inbound/inbound/' \
  "12 objects · $count emails\$" \
  '[0-9a-z]{40} +[0-9.]+ (B|KiB) +email *$' \
  'export-2026-09\.csv +32 B *$' \
  'receipt-rule\.json +38 B *$'

step "the saved inbox, every row loaded" \
  'saved mail-inbound/inbound/ as the inbox' \
  "${logo}Inbox s3://mail-inbound/inbound/ · $count messages · 2 not email" \
  "$header" \
  "${rows[@]}" \
  '!loading…'

step "the opened message" \
  "${logo}Message s3://mail-inbound/inbound/5c7e9g1i3k5m7o9q1s3u5w7y9a1c3e5g7i9k1m3o" \
  '^From: Mei Chen <mei\.chen@example\.com>' \
  '^To: mail@example\.org *$' \
  '^Date: [A-Z][a-z]{2}, [0-9]{1,2} [A-Z][a-z]{2} [0-9]{4} [0-9]{2}:[0-9]{2}:[0-9]{2} \+0000 *$' \
  '^Subject: Draft agenda for Thursday.s planning call' \
  '^Hi all,' \
  '^3\. Hiring' \
  'Finish the reporting export \(Sam owns this\)'

step "the message, scrolled" \
  "${logo}Message s3://mail-inbound/inbound/" \
  '!^Reply-To:' \
  'Keys for the new space arrive on the 3rd' \
  'Desks go in over the weekend, so pack up by Friday' \
  '^Cheers,'

step "back in the inbox" \
  "${logo}Inbox s3://" \
  "$header" \
  "${rows[@]}" \
  '!Message s3://'

if matches "${frames[$last]}" "${logo}Inbox s3://" "$header" "${rows[@]}" '!Message s3://'; then
  echo "ok    the last snapshot is the inbox (snapshot $last)"
else
  echo "FAIL  the last snapshot is not the inbox" >&2
  failed=1
fi

# Case-insensitive, and including the phrases the app itself puts on the status line.
errors='panicked|error|could not|no longer exists|unexpected reply|nothing matches|not connected|no account is connected|open a bucket first|which is not in|denied|failed|does not exist'
s=0
bad=$(grep -EHin -- "$errors" "${frames[@]}") || s=$?
if [ "$s" -eq 0 ]; then
  echo "FAIL  error text on screen:" >&2
  echo "$bad" | sed "s|$dir/frames/||" >&2
  failed=1
elif [ "$s" -eq 1 ]; then
  echo "ok    no error text in any snapshot"
else
  echo "FAIL  grep for error text exited $s" >&2
  exit 2
fi

if [ "$failed" -ne 0 ]; then
  echo "the recording does not show the demo" >&2
  exit 1
fi
echo "all $((last + 1)) snapshots checked, $count inbox rows found"
