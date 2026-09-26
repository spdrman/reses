#!/usr/bin/env python3
"""Reference decodes of mail fixtures, for tests/mail_oracle.rs and the committed goldens.

reses decodes mail with the mail-parser crate. So that no expected output ever comes from the
decoder under test, this script builds the same views from Python's standard `email` package
(policy.default), following the rules reses documents in src/mail.rs:

    mail_oracle.py render FILE          what `reses FILE` prints
    mail_oracle.py render --html FILE   what `reses --html FILE` prints
    mail_oracle.py saved FILE           what `--save-attachments` writes, one "name<TAB>size<TAB>sha256"
                                        line per file, in order
    mail_oracle.py html-source FILE     the HTML part a plain render would convert, decoded, so a
                                        body converted from HTML can be pinned by hand

The layout is From, Reply-To, To, Cc, Bcc, Date, Subject, Message-ID, an Attachments line when
there are attachments, "Message:", a blank line and the body.

Python's parser is the authority on the parse tree. The rules on top of it are reses's own, and
where a rule follows a standard that Python doesn't (the RFC 5322 two-digit year, zone names), the
comment on that code says so. Where the parse tree itself disagrees with the standards, this
script doesn't paper over it: the fixture's golden is pinned by hand instead, and
tests/fixtures/mail/HAND-PINNED says why.

The one thing this script can't do is turn HTML into text the way reses's HTML crate does, so
when a plain render's body comes from an HTML part it prints a marker line in place of the body,
and that golden's body is pinned by hand.
"""

import datetime
import hashlib
import re
import sys
import unicodedata
from email import policy
from email.message import EmailMessage
from email.parser import BytesParser
from email.utils import _parsedate_tz, getaddresses

HTML_MARKER = "<<body converted from HTML; pinned by hand>>"

# Characters that make a display name need quoting when it's printed back.
NAME_SPECIALS = set(',;:<>@"()[]\\')

# Zone names RFC 5322 defines (4.3), and a table of common ones it doesn't. RFC 5322 says an
# unknown zone name means "-0000"; the extra table is the out-of-band knowledge it allows for.
RFC_ZONES = {"UT": 0, "GMT": 0, "Z": 0, "EST": -300, "EDT": -240, "CST": -360, "CDT": -300,
             "MST": -420, "MDT": -360, "PST": -480, "PDT": -420}
EXTRA_ZONES = {"UTC": 0, "WET": 0, "WEST": 60, "BST": 60, "CET": 60, "CEST": 120, "EET": 120,
               "EEST": 180, "MSK": 180, "HKT": 480, "SGT": 480, "AWST": 480, "JST": 540,
               "KST": 540, "ACST": 570, "AEST": 600, "AEDT": 660, "NZST": 720, "NZDT": 780,
               "HST": -600, "AKST": -540, "AKDT": -480}

# Extensions for the names reses makes up for attachments that don't carry one.
EXTENSIONS = {"message/rfc822": "eml", "message/global": "eml", "message/delivery-status": "txt",
              "text/plain": "txt", "text/html": "html", "text/calendar": "ics",
              "application/pdf": "pdf", "image/png": "png", "image/jpeg": "jpg",
              "image/gif": "gif", "application/zip": "zip"}

MAX_NAME_BYTES = 200
MAX_SAVED = 1000
BIDI = {0x61C, 0x200E, 0x200F} | set(range(0x202A, 0x202F)) | set(range(0x2066, 0x206A))


class Part(EmailMessage):
    """A message part that reads message/* bodies as opaque bytes.

    Python normally parses a message/rfc822 body into a nested message and can't give back the
    bytes it came from. reses saves a forwarded message as the original bytes, so here the
    parser sees every message/* part as a leaf and keeps its body as it was.
    """

    def real_type(self):
        return EmailMessage.get_content_type(self)

    def get_content_type(self):
        ctype = EmailMessage.get_content_type(self)
        return "application/x-opaque-message" if ctype.startswith("message/") else ctype


def parse(raw):
    # reses tolerates a UTF-8 byte order mark and blank lines before the first header, which
    # some tools write when they save a message.
    if raw.startswith(b"\xef\xbb\xbf"):
        raw = raw[3:]
    raw = re.sub(rb"\A(?:\r\n|\n|\r)+", b"", raw)
    return BytesParser(_class=Part, policy=policy.default).parsebytes(raw)


def header(msg, name):
    """The first header called `name`, allowing blanks before the colon (obsolete syntax that
    RFC 5322 4.5 says to accept)."""
    for key, value in msg.items():
        if key.strip().lower() == name.lower():
            return value
    return None


def raw_header(msg, name):
    """The first `name` header as written, with the line breaks of its folding taken out."""
    for key, value in msg.raw_items():
        if key.strip().lower() == name.lower():
            return sanitize(re.sub(r"\r\n|\r|\n", "", value)).strip()
    return None


def sanitize(text):
    """Raw 8-bit bytes in a header, read back as UTF-8 with bad sequences replaced, the way
    Python's own header objects treat them."""
    return text.encode("utf-8", "surrogateescape").decode("utf-8", "replace")


def quote_name(name):
    if any(c in NAME_SPECIALS for c in name):
        return '"' + name.replace("\\", "\\\\").replace('"', '\\"') + '"'
    return name


def mailbox(address):
    spec = "" if address.addr_spec == "<>" else address.addr_spec
    name = address.display_name
    if name and spec:
        return f"{quote_name(name)} <{spec}>"
    return quote_name(name) if name else spec


def render_addresses(value):
    if value is None or not hasattr(value, "groups"):
        return str(value or "").strip()
    out = []
    for group in value.groups:
        members = [mailbox(a) for a in group.addresses]
        if group.display_name is None:
            out.extend(m for m in members if m)
        else:
            inner = ", ".join(m for m in members if m)
            out.append(f"{quote_name(group.display_name)}: {inner};" if inner
                       else f"{quote_name(group.display_name)}:;")
    return ", ".join(out)


def zone_minutes(raw):
    """The zone of a Date value in minutes east of UTC, None for an unknown zone ("-0000"), or
    False when the zone is a number no real zone can have."""
    no_comments = re.sub(r"\([^)]*\)", " ", raw).split()
    if not no_comments:
        return None
    token = no_comments[-1]
    if re.fullmatch(r"[+-]\d+", token) and len(token) != 5:
        return False
    m = re.fullmatch(r"([+-])(\d\d)(\d\d)", token)
    if m:
        hours, minutes = int(m.group(2)), int(m.group(3))
        if hours > 23 or minutes > 59:
            return False
        if token == "-0000":
            return None
        return (1 if m.group(1) == "+" else -1) * (hours * 60 + minutes)
    upper = token.upper()
    if upper in RFC_ZONES:
        return RFC_ZONES[upper]
    return EXTRA_ZONES.get(upper)


def render_date(msg):
    raw = raw_header(msg, "date")
    if not raw:
        return ""
    parsed = _parsedate_tz(raw)
    zone = zone_minutes(raw)
    if parsed is None or zone is False:
        return raw
    year, month, day, hour, minute, second = parsed[:6]
    # RFC 5322 4.3 reads two-digit years 00-49 as 20xx and 50-99 as 19xx. Python's parser
    # pivots at 69 instead, so undo that and apply the standard's rule.
    if 2050 <= year <= 2068 and str(year) not in raw:
        year -= 100
    try:
        when = datetime.datetime(year, month, day, hour, minute, second)
    except ValueError:
        return raw
    if zone is None:
        stamp = "-0000"
    else:
        sign = "+" if zone >= 0 else "-"
        stamp = f"{sign}{abs(zone) // 60:02d}{abs(zone) % 60:02d}"
    return when.strftime("%a, %d %b %Y %H:%M:%S ") + stamp


RECEIVED_FOR = re.compile(r"\bfor\s+<?([^\s<>;]+@[^\s<>;]+)>?", re.IGNORECASE)


def addresses(msg, name):
    values = [str(v) for k, v in msg.items() if k.strip().lower() == name.lower()]
    return [a for _, a in getaddresses(values) if a]


def bcc(msg):
    """An explicit Bcc header if it has anything in it, otherwise every envelope recipient that
    isn't already in To or Cc, deduplicated without regard to case."""
    explicit = render_addresses(header(msg, "bcc"))
    if explicit:
        return explicit
    found = []
    for name in ("Delivered-To", "X-Original-To", "Envelope-To"):
        found += addresses(msg, name)
    for key, value in msg.raw_items():
        if key.strip().lower() == "received":
            found += RECEIVED_FOR.findall(sanitize(re.sub(r"\r\n|\r|\n", "", value)))
    visible = {a.lower() for a in addresses(msg, "To") + addresses(msg, "Cc")}
    seen, out = set(), []
    for a in found:
        if a.lower() not in seen and a.lower() not in visible:
            seen.add(a.lower())
            out.append(a)
    return ", ".join(out)


def leaves(msg):
    """Every part that isn't a multipart container, depth first, in the order they appear."""
    return [p for p in msg.walk() if not p.is_multipart()]


def is_attachment(part):
    """A part is an attachment if it says so, if it has a file name, or if it's anything other
    than plain text or HTML. That catches Apple Mail's inline PDFs, S/MIME signatures, forwarded
    messages, delivery reports and nameless files, and keeps a named text file from ever being
    taken for the message body."""
    if part.get_content_disposition() == "attachment":
        return True
    if part.get_filename():
        return True
    return part.real_type() not in ("text/plain", "text/html")


def transfer_decoded(part):
    return part.get_payload(decode=True) or b""


def text_of(part):
    """A text part's body as a string: the declared charset, or with none declared, UTF-8 if the
    bytes are valid UTF-8 and Windows-1252 if not (8-bit text with no charset is common, and
    losing it is worse than guessing)."""
    data = transfer_decoded(part)
    charset = part.get_content_charset()
    if charset is None:
        try:
            return data.decode("utf-8")
        except UnicodeDecodeError:
            return data.decode("cp1252", errors="replace")
    try:
        return data.decode(charset, errors="replace")
    except LookupError:
        return data.decode("utf-8", errors="replace")


def body(msg, prefer_html):
    candidates = [p for p in leaves(msg) if not is_attachment(p)]
    plain = [p for p in candidates if p.real_type() == "text/plain"]
    html = [p for p in candidates if p.real_type() == "text/html"]
    if prefer_html:
        chosen = (html or plain or [None])[0]
    else:
        if not plain and html:
            return HTML_MARKER
        chosen = (plain or [None])[0]
    if chosen is None:
        return ""
    return text_of(chosen).replace("\r\n", "\n").strip()


def attachments(msg):
    """(name, bytes) for each attachment. A part without a name gets attachment-N and an
    extension from its type, N counting attachments from 1."""
    out = []
    for part in leaves(msg):
        if not is_attachment(part):
            continue
        name = part.get_filename()
        if not name:
            ext = EXTENSIONS.get(part.real_type(), "bin")
            name = f"attachment-{len(out) + 1}.{ext}"
        out.append((name, transfer_decoded(part)))
    return out


def render(raw, prefer_html):
    msg = parse(raw)
    lines = [
        "From: " + render_addresses(header(msg, "from")),
        "Reply-To: " + render_addresses(header(msg, "reply-to")),
        "To: " + render_addresses(header(msg, "to")),
        "Cc: " + render_addresses(header(msg, "cc")),
        "Bcc: " + bcc(msg),
        "Date: " + render_date(msg),
        "Subject: " + str(header(msg, "subject") or "").strip(),
        "Message-ID: " + (raw_header(msg, "message-id") or ""),
    ]
    files = attachments(msg)
    if files:
        lines.append("Attachments: " + ", ".join(f"{n} ({len(b)} bytes)" for n, b in files))
    lines += ["Message:", "", body(msg, prefer_html)]
    # Display names inside address objects keep raw 8-bit bytes escaped, unlike the header
    # strings themselves, so the whole render goes through the same repair.
    return sanitize("\n".join(lines) + "\n")


def safe_name(name):
    """The file name reses saves under: the last path component, controls and bidi formatting
    characters replaced, at most 200 bytes with the extension kept."""
    parts = [p for p in re.split(r"[/\\]", name) if p and p != "."]
    base = parts[-1] if parts else ""
    if not base or base == "..":
        return "attachment"
    clean = "".join("_" if unicodedata.category(c) == "Cc" or ord(c) in BIDI else c for c in base)
    if len(clean.encode()) <= MAX_NAME_BYTES:
        return clean
    stem, suffix = split_suffix(clean)
    if len(suffix.encode()) > 32:
        stem, suffix = clean, ""
    budget = MAX_NAME_BYTES - len(suffix.encode())
    cut = stem.encode()[:budget].decode("utf-8", errors="ignore")
    return cut + suffix


def split_suffix(name):
    i = name.rfind(".")
    if 0 < i < len(name) - 1:
        return name[:i], name[i:]
    return name, ""


def saved(raw):
    taken, next_n, lines = set(), {}, []
    for name, data in attachments(parse(raw))[:MAX_SAVED]:
        name = safe_name(name)
        stem, suffix = split_suffix(name)
        n = next_n.get(name, 0)
        while True:
            candidate = name if n == 0 else f"{stem}-{n}{suffix}"
            n += 1
            if candidate not in taken:
                break
        next_n[name] = n
        taken.add(candidate)
        lines.append(f"{candidate}\t{len(data)}\t{hashlib.sha256(data).hexdigest()}\n")
    return "".join(lines)


def main(argv):
    if len(argv) >= 2 and argv[0] == "render":
        prefer_html = argv[1] == "--html"
        path = argv[2] if prefer_html else argv[1]
        sys.stdout.buffer.write(render(open(path, "rb").read(), prefer_html).encode("utf-8"))
    elif len(argv) == 2 and argv[0] == "html-source":
        msg = parse(open(argv[1], "rb").read())
        html = [p for p in leaves(msg) if not is_attachment(p) and p.real_type() == "text/html"]
        sys.stdout.buffer.write(text_of(html[0]).encode("utf-8") if html else b"")
    elif len(argv) == 2 and argv[0] == "saved":
        sys.stdout.buffer.write(saved(open(argv[1], "rb").read()).encode("utf-8"))
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv[1:])
