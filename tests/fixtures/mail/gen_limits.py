#!/usr/bin/env python3
"""Write the recursion-limit fixtures. Run it inside the CI container:
    scripts/ci-docker.sh --exec 'python3 tests/fixtures/mail/gen_limits.py'

For each nesting shape, LIMITS holds the deepest nesting python/reses.py still decodes in the CI
image (found by bisection). limit-<shape>.eml sits exactly at that depth and gets goldens like any
other fixture. beyond-python/<shape>.eml is one level deeper, where reses.py dies with a
RecursionError, so it has no golden; regen.sh checks that Python really does fail on it.
"""
import os

LIMITS = {
    "from-nested": 486,
    "from-unclosed": 240,
    "msgid-comment": 242,
    "ctype-comment": 483,
    "envelope-comment": 493,
    "envelope-group": 985,
    "rfc822": 972,
    "multipart": 970,
}

BASE = "To: b@example.org\nSubject: nesting fixture\n"


def message(shape, n):
    if shape == "from-nested":
        return f"From: {'(' * n}c{')' * n} <a@example.com>\n{BASE}\nbody\n"
    if shape == "from-unclosed":
        return f"From: x {'(' * n}\n{BASE}\nbody\n"
    if shape == "msgid-comment":
        return f"From: a@example.com\n{BASE}Message-ID: {'(' * n}{')' * n}<i@example.com>\n\nbody\n"
    if shape == "ctype-comment":
        return (f"From: a@example.com\n{BASE}"
                f"Content-Type: text/plain; charset=utf-8 {'(' * n}{')' * n}\n\nbody\n")
    if shape == "envelope-comment":
        return f"From: a@example.com\n{BASE}Delivered-To: {'(' * n}{')' * n} h@example.org\n\nbody\n"
    if shape == "envelope-group":
        return f"From: a@example.com\n{BASE}Delivered-To: {'g:' * n}h@example.org{';' * n}\n\nbody\n"
    if shape == "rfc822":
        return ("From: a@example.com\n" + BASE + "Content-Type: message/rfc822\n\n" * n
                + "Subject: inner\n\ninner body\n")
    if shape == "multipart":
        head = "".join(f"Content-Type: multipart/mixed; boundary=b{k}\n\n--b{k}\n" for k in range(n))
        tail = "".join(f"\n--b{k}--\n" for k in reversed(range(n)))
        return "From: a@example.com\n" + BASE + head + "Content-Type: text/plain\n\nleaf\n" + tail
    raise ValueError(shape)


here = os.path.dirname(os.path.abspath(__file__))
for shape, n in LIMITS.items():
    with open(os.path.join(here, f"limit-{shape}.eml"), "w") as f:
        f.write(message(shape, n))
    with open(os.path.join(here, "beyond-python", f"{shape}.eml"), "w") as f:
        f.write(message(shape, n + 1))
