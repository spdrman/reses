#!/usr/bin/env python3
"""Turn a raw stored message (SMTP/RFC 5322, e.g. an Amazon SES S3 object) into a
readable email with From, To, Cc, Bcc, Subject, Date and the message body.

Usage:
    reses FILE [FILE ...]            print each message to stdout
    reses FILE -o out.txt            write to a file
    reses FILE --html                show the HTML part instead of plain text
    reses FILE --save-attachments D  write attachments into directory D
    cat FILE | reses                 read from stdin
"""

import argparse
import html
import re
import sys
from email import policy
from email.parser import BytesParser
from email.utils import getaddresses, parsedate_to_datetime
from pathlib import Path

HEADER_ORDER = ["From", "Reply-To", "To", "Cc", "Bcc", "Date", "Subject", "Message-ID"]

# "for <addr>" / "for addr;" inside a Received header is the envelope recipient.
RECEIVED_FOR = re.compile(r"\bfor\s+<?([^\s<>;]+@[^\s<>;]+)>?", re.IGNORECASE)


def parse(data: bytes):
    return BytesParser(policy=policy.default).parsebytes(data)


def addresses(msg, name):
    return [a for _, a in getaddresses(msg.get_all(name, [])) if a]


def envelope_recipients(msg):
    """Recipients the message was actually delivered to, from the transport headers."""
    found = []
    for name in ("Delivered-To", "X-Original-To", "Envelope-To"):
        found += addresses(msg, name)
    for received in msg.get_all("Received", []):
        found += RECEIVED_FOR.findall(str(received))
    seen, out = set(), []
    for addr in found:
        key = addr.lower()
        if key not in seen:
            seen.add(key)
            out.append(addr)
    return out


def bcc(msg):
    """An explicit Bcc header if present, otherwise any envelope recipient not in To/Cc."""
    explicit = msg.get("Bcc")
    if explicit:
        return str(explicit)
    visible = {a.lower() for a in addresses(msg, "To") + addresses(msg, "Cc")}
    hidden = [a for a in envelope_recipients(msg) if a.lower() not in visible]
    return ", ".join(hidden)


def html_to_text(markup):
    markup = re.sub(r"(?is)<(script|style).*?</\1>", "", markup)
    markup = re.sub(r"(?i)<br\s*/?>", "\n", markup)
    markup = re.sub(r"(?i)</(p|div|tr|li|h[1-6])>", "\n", markup)
    text = html.unescape(re.sub(r"<[^>]+>", "", markup))
    return re.sub(r"\n{3,}", "\n\n", text).strip()


def body(msg, prefer_html=False):
    preference = ("html", "plain") if prefer_html else ("plain", "html")
    part = msg.get_body(preferencelist=preference)
    if part is None:
        return ""
    content = part.get_content()
    if part.get_content_subtype() == "html" and not prefer_html:
        content = html_to_text(content)
    return content.replace("\r\n", "\n").strip()


def attachments(msg):
    return [p for p in msg.iter_attachments() if p.get_filename()]


def format_message(msg, prefer_html=False):
    values = {
        "From": msg.get("From", ""),
        "Reply-To": msg.get("Reply-To", ""),
        "To": msg.get("To", ""),
        "Cc": msg.get("Cc", ""),
        "Bcc": bcc(msg),
        "Date": msg.get("Date", ""),
        "Subject": msg.get("Subject", ""),
        "Message-ID": msg.get("Message-ID", ""),
    }
    date = values["Date"]
    if date:
        try:
            values["Date"] = parsedate_to_datetime(str(date)).strftime("%a, %d %b %Y %H:%M:%S %z")
        except (TypeError, ValueError):
            pass

    lines = [f"{name}: {str(values[name]).strip()}" for name in HEADER_ORDER]
    files = attachments(msg)
    if files:
        listed = ", ".join(f"{p.get_filename()} ({len(p.get_payload(decode=True) or b'')} bytes)" for p in files)
        lines.append(f"Attachments: {listed}")
    lines.append("Message:")
    lines.append("")
    lines.append(body(msg, prefer_html))
    return "\n".join(lines) + "\n"


def save_attachments(msg, directory: Path):
    directory.mkdir(parents=True, exist_ok=True)
    saved = []
    for part in attachments(msg):
        name = Path(part.get_filename()).name or "attachment"
        target = directory / name
        n = 1
        while target.exists():
            target = directory / f"{Path(name).stem}-{n}{Path(name).suffix}"
            n += 1
        payload = part.get_payload(decode=True) or b""
        target.write_bytes(payload)
        saved.append(target)
    return saved


def main(argv=None):
    ap = argparse.ArgumentParser(prog="reses", description=__doc__.split("\n\n")[0])
    ap.add_argument("files", nargs="*", help="raw message files (default: stdin)")
    ap.add_argument("-o", "--output", help="write to this file instead of stdout")
    ap.add_argument("--html", action="store_true", help="show the HTML part instead of plain text")
    ap.add_argument("--save-attachments", metavar="DIR", help="write attachments into DIR")
    args = ap.parse_args(argv)

    sources = [(f, Path(f).read_bytes()) for f in args.files] or [("<stdin>", sys.stdin.buffer.read())]

    chunks = []
    for name, data in sources:
        msg = parse(data)
        text = format_message(msg, args.html)
        if len(sources) > 1:
            text = f"==> {name} <==\n{text}"
        chunks.append(text)
        if args.save_attachments:
            for path in save_attachments(msg, Path(args.save_attachments)):
                print(f"saved {path}", file=sys.stderr)

    out = "\n".join(chunks)
    if args.output:
        Path(args.output).write_text(out, encoding="utf-8")
    else:
        sys.stdout.write(out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
