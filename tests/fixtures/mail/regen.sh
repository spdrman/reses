#!/usr/bin/env bash
# Regenerate the golden outputs from python/reses.py. Run it inside the CI container:
#   scripts/ci-docker.sh --exec tests/fixtures/mail/regen.sh
# For each NAME.eml it writes NAME.out (plain), NAME.html.out (--html) and NAME.saved, which
# lists what --save-attachments writes, in order, as "file<TAB>size<TAB>sha256".
set -euo pipefail
cd "$(dirname "$0")"
reses=../../../python/reses.py
for f in *.eml; do
  base=${f%.eml}
  python3 "$reses" "$f" > "$base.out"
  python3 "$reses" --html "$f" > "$base.html.out"
  dir=$(mktemp -d)
  python3 "$reses" --save-attachments "$dir" "$f" 2>&1 >/dev/null | while read -r _ path; do
    printf '%s\t%s\t%s\n' "$(basename "$path")" "$(stat -c %s "$path")" "$(sha256sum < "$path" | cut -d' ' -f1)"
  done > "$base.saved"
  rm -rf "$dir"
done

# One level past each limit reses.py dies, which is the point of these fixtures, so there is no
# golden. Make sure it still dies; if it ever stops dying, the fixture belongs in the main set.
for f in beyond-python/*.eml; do
  if python3 "$reses" "$f" >/dev/null 2>&1; then
    echo "$f: reses.py decoded it, so it is not beyond the limit" >&2
    exit 1
  fi
done
