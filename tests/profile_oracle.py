#!/usr/bin/env python3
"""Reference reads of the AWS credentials file, for tests/profile_oracle.rs.

reses reads profiles through aws-config, but it also writes new ones into ~/.aws/credentials,
and the AWS CLI reads that file through botocore, which uses Python's configparser. So
configparser is the authority on whether a file reses writes still works for the CLI. This
script prints what RawConfigParser makes of a file, and tests/profile_oracle.rs uses it to check
the writer's output and when reses refuses to save.

    profile_oracle.py dump FILE    strict and non-strict reads, every section and item

Every string is printed as the hex of its UTF-8 bytes ("-" for an empty string), so nothing
ever needs escaping.
"""

import configparser
import sys


def h(s):
    """I hex-encode a string so the Rust side never has to unescape anything."""
    return s.encode("utf-8").hex() or "-"


def read(path, strict):
    """I read a file the way botocore does, strict or not."""
    cp = configparser.RawConfigParser(strict=strict)
    # botocore calls cp.read([path]), which opens the file in text mode like this does, so the
    # same universal-newline splitting applies.
    with open(path, encoding="utf-8") as f:
        cp.read_file(f)
    return cp


def error_line(e):
    """I name a configparser error, with the section and key it points at when it has them."""
    name = type(e).__name__
    if isinstance(e, configparser.DuplicateSectionError):
        return f"{name} {h(e.section)}"
    if isinstance(e, configparser.DuplicateOptionError):
        return f"{name} {h(e.section)} {h(e.option)}"
    return name


def dump(path):
    """I print the strict verdict, then every section and item the non-strict read sees."""
    out = []
    try:
        read(path, True)
        out.append("strict ok")
    except configparser.Error as e:
        out.append("strict " + error_line(e))
    try:
        cp = read(path, False)
    except configparser.Error as e:
        out.append("nonstrict " + error_line(e))
        return out
    out.append("nonstrict ok")
    out.append("defaults")
    for k, v in cp.defaults().items():
        out.append(f"item {h(k)} {h(v)}")
    for s in cp.sections():
        out.append(f"section {h(s)}")
        for k, v in cp.items(s, raw=True):
            out.append(f"item {h(k)} {h(v)}")
    return out


def main(argv):
    """I only take `dump FILE`; anything else prints the usage and exits non-zero."""
    if len(argv) == 3 and argv[1] == "dump":
        print("\n".join(dump(argv[2])))
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv)
