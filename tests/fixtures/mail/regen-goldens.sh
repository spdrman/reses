#!/usr/bin/env bash
# Rewrite the mail goldens from tests/mail_oracle.py, which reads each fixture with Python's
# standard email package. Run it inside the CI container:
#   scripts/ci-docker.sh --exec tests/fixtures/mail/regen-goldens.sh
# For each NAME.eml it writes NAME.out (plain), NAME.html.out (--html) and NAME.saved (what
# --save-attachments writes). Goldens listed in HAND-PINNED are left alone: those are the cases
# where the standards and Python's parser disagree, or where the body comes from HTML, and
# tests/mail_oracle.rs keeps checking whatever part of them the oracle can still speak for.
set -euo pipefail
cd "$(dirname "$0")"
oracle=../../mail_oracle.py
pinned() { grep -q "^$1 " HAND-PINNED; }
for f in *.eml; do
  base=${f%.eml}
  pinned "$base.out" || python3 "$oracle" render "$f" > "$base.out"
  pinned "$base.html.out" || python3 "$oracle" render --html "$f" > "$base.html.out"
  pinned "$base.saved" || python3 "$oracle" saved "$f" > "$base.saved"
done
