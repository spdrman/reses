#!/usr/bin/env python3
"""Reference reads of AWS INI files, for tests/profile_oracle.rs.

botocore, and so the AWS CLI, reads ~/.aws/credentials and ~/.aws/config with Python's
configparser, which makes configparser the authority on what those files mean. This script
prints what RawConfigParser makes of a file, in a line format that tests/profile_oracle.rs
rebuilds from reses's own parser and compares byte for byte.

    profile_oracle.py dump FILE              strict and non-strict reads, every section and item
    profile_oracle.py region FILE PROFILE    the region botocore resolves for PROFILE in a config file

Every string is printed as the hex of its UTF-8 bytes ("-" for an empty string), so nothing
ever needs escaping.
"""

import configparser
import shlex
import sys


def h(s):
    return s.encode("utf-8").hex() or "-"


def read(path, strict):
    cp = configparser.RawConfigParser(strict=strict)
    # botocore calls cp.read([path]), which opens the file in text mode like this does, so the
    # same universal-newline splitting applies.
    with open(path, encoding="utf-8") as f:
        cp.read_file(f)
    return cp


def error_line(e):
    name = type(e).__name__
    if isinstance(e, configparser.DuplicateSectionError):
        return f"{name} {h(e.section)}"
    if isinstance(e, configparser.DuplicateOptionError):
        return f"{name} {h(e.section)} {h(e.option)}"
    return name


def dump(path):
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


def region(path, profile):
    # botocore's raw_config_parse uses a strict parser and gives up on the whole file when it
    # raises, so no region comes out of a file configparser rejects.
    try:
        cp = read(path, True)
    except configparser.Error:
        return "none"
    # This loop is botocore's build_profile_map (botocore/configloader.py): `[profile NAME]`
    # must shlex-split into exactly two words, and `[default]` counts as a profile too. A later
    # section with the same profile name replaces an earlier one.
    profiles = {}
    for key in cp.sections():
        values = dict(cp.items(key, raw=True))
        if key.startswith("profile"):
            try:
                parts = shlex.split(key)
            except ValueError:
                continue
            if len(parts) == 2:
                profiles[parts[1]] = values
        elif key == "default":
            profiles[key] = values
    value = profiles.get(profile, {}).get("region")
    # botocore would hand back "" or a nested block; reses treats both as "no region".
    if not value or value.startswith("\n"):
        return "none"
    return h(value)


def main(argv):
    if len(argv) == 3 and argv[1] == "dump":
        print("\n".join(dump(argv[2])))
    elif len(argv) == 4 and argv[1] == "region":
        print(region(argv[2], argv[3]))
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv)
