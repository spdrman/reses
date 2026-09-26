#!/usr/bin/env bash
# Seed a throwaway MinIO with a synthetic SES inbox for the README demo. record.sh runs this
# inside the CI image on the demo's private network; it never talks to anything else.
#
# Every sender, recipient and subject is invented, and every address is on example.com,
# example.org or example.net. The keys are MinIO's fixed test values from record.sh.
#
#   S3_ENDPOINT   e.g. http://minio:9000
#   S3_KEY        access key
#   S3_SECRET     secret key
#   MESSAGES      the message list, demo/messages.tsv
set -euo pipefail

: "${S3_ENDPOINT:?}" "${S3_KEY:?}" "${S3_SECRET:?}" "${MESSAGES:?}"
BUCKET=mail-inbound
PREFIX=inbound/
TO=mail@example.org

s3() {
  # curl signs the request itself, so seeding needs no S3 client beyond what the CI image has.
  curl -fsS --aws-sigv4 "aws:amz:us-east-1:s3" --user "$S3_KEY:$S3_SECRET" \
    -H "x-amz-content-sha256: UNSIGNED-PAYLOAD" "$@"
}

for i in $(seq 1 60); do
  curl -fs -o /dev/null "$S3_ENDPOINT/minio/health/live" && break
  [ "$i" = 60 ] && { echo "MinIO never came up" >&2; exit 1; }
  sleep 1
done

s3 -X PUT "$S3_ENDPOINT/$BUCKET" -o /dev/null
s3 -X PUT "$S3_ENDPOINT/site-assets" -o /dev/null
s3 -X PUT "$S3_ENDPOINT/backups-2026" -o /dev/null

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

put() { # put KEY FILE
  s3 -T "$2" "$S3_ENDPOINT/$BUCKET/$1" -o /dev/null
}

# The body of each message in demo/messages.tsv, by its id.
body() {
  case $1 in
    3k8v*) echo "This week: six soldering stations under the bench light, and the one we kept." ;;
    7d2f*) echo "Hi, the renewal paperwork for 4B is on the portal. Could you sign by Friday?" ;;
    1a3c*) echo "Uploaded the photos from Saturday. The one of the lathe demo came out great." ;;
    9z7x*) echo "Thanks for stopping by. 2 x flat white, 1 x almond croissant. Total 14.50." ;;
    4b6d*) echo "We noticed a new sign-in to your account. If this was you, there's nothing to do." ;;
    6m4k*) echo "The ridge loop is 14 km with one steep bit. The lake trail is flatter. Your call!" ;;
    2p4r*) echo "All 1,204 tests passed in 6m 12s." ;;
    8q6o*) echo "Hi, invoice 4471 for September is ready. Net 30 as usual. Thanks!" ;;
    0e2g*) echo "Welcome! Here are a few notes to get you going in your first week." ;;
    # The one the demo opens and scrolls, so it's long enough to scroll.
    5c7e*) cat <<'EOF' ;;
Hi all,

Here's a draft agenda for Thursday's planning call. Shout if I missed anything
and I'll fold it in before the invite goes out.

1. Where we landed last quarter
   - Shipped the new onboarding flow two weeks early
   - Support tickets about sign-up are down by a third
   - Still behind on the reporting export

2. Priorities for next quarter
   - Finish the reporting export (Sam owns this)
   - Start the offline mode spike
   - Tidy up the settings screen before it grows any further

3. Hiring
   - One backend role is open, interviews start next week
   - We'd like two people from the team on each panel

4. The office move
   - Keys for the new space arrive on the 3rd
   - Desks go in over the weekend, so pack up by Friday

5. Anything else

I've booked the big room from 10 to 11, and there'll be coffee.

Cheers,
Mei
EOF
    *) echo "no body for $1" >&2; exit 1 ;;
  esac
}

# msg ID AGE NAME ADDRESS SUBJECT. AGE is anything `date -d` takes, so the inbox shows a spread
# of recent dates whenever the demo is recorded. SES keys each object by its message id.
msg() {
  local id=$1 age=$2 from="$3 <$4>" subject=$5 domain=${4##*@} date
  date=$(date -u -R -d "$age")
  {
    printf 'Return-Path: <bounce-%s@%s>\n' "$id" "$domain"
    printf 'Received: from mail.%s (mail.%s [192.0.2.25])\n' "$domain" "$domain"
    printf ' by inbound-smtp.us-east-1.example.net with SMTP id %s\n' "$id"
    printf ' for %s;\n %s\n' "$TO" "$date"
    printf 'X-SES-Spam-Verdict: PASS\nX-SES-Virus-Verdict: PASS\n'
    printf 'Received-SPF: pass (spfCheck: domain of %s designates 192.0.2.25 as permitted sender)\n' "$domain"
    printf 'Authentication-Results: amazonses.example.net;\n spf=pass;\n dkim=pass header.i=@%s;\n dmarc=pass header.from=%s;\n' "$domain" "$domain"
    printf 'X-SES-RECEIPT: AEFBQUFBQUFBQUFFexampleexampleexampleexample\n'
    printf 'MIME-Version: 1.0\n'
    printf 'From: %s\n' "$from"
    printf 'To: %s\n' "$TO"
    printf 'Date: %s\n' "$date"
    printf 'Subject: %s\n' "$subject"
    printf 'Message-ID: <%s@%s>\n' "$id" "$domain"
    printf 'Content-Type: text/plain; charset="UTF-8"\n\n'
    body "$id"
  } | sed 's/$/\r/' >"$tmp/$id"
  put "$PREFIX$id" "$tmp/$id"
}

# The inbox lists newest upload first, and the table is oldest first, so upload in its order.
while IFS=$'\t' read -r id age name address subject; do
  case $id in '#'* | '') continue ;; esac
  msg "$id" "$age" "$name" "$address" "$subject"
done <"$MESSAGES"

# Not every object in a mail folder is mail, so the browser's marks have something to tell apart.
printf 'bucket,messages\nmail-inbound,10\n' >"$tmp/export.csv"
put "${PREFIX}export-2026-09.csv" "$tmp/export.csv"
printf '{"rule":"store-to-s3","enabled":true}\n' >"$tmp/rule.json"
put "${PREFIX}receipt-rule.json" "$tmp/rule.json"
printf 'Weekly bounce summary: 0 bounces, 0 complaints.\n' >"$tmp/summary.txt"
put "reports/bounces-week-38.txt" "$tmp/summary.txt"

echo "seeded s3://$BUCKET/$PREFIX"
