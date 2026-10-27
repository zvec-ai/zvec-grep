#!/usr/bin/env python3
"""Reject strong glibc imports above the supported baseline in an ELF payload."""

import argparse
from pathlib import Path
import re
import subprocess


def required_versions(symbols):
    versions = set()
    for line in symbols.splitlines():
        # Weak imports such as Rust's optional pidfd_spawnp@GLIBC_2.39 do not
        # establish a runtime floor. Undefined strong imports do.
        fields = line.split()
        if "UND" not in fields or "WEAK" in fields:
            continue
        versions.update(tuple(map(int, value.split(".")))
                        for value in re.findall(r"@GLIBC_([0-9]+(?:\.[0-9]+)+)", line))
    return versions


def audit(path, maximum, readelf="readelf"):
    paths = [path] if path.is_file() else sorted(p for p in path.rglob("*") if p.is_file())
    checked = 0
    failures = []
    for library in paths:
        with library.open("rb") as stream:
            if stream.read(4) != b"\x7fELF":
                continue
        checked += 1
        symbols = subprocess.check_output([readelf, "--dyn-syms", "--wide", str(library)], text=True)
        versions = required_versions(symbols)
        highest = max(versions, default=(0,))
        print(f"{library}: GLIBC_{'.'.join(map(str, highest))}")
        if highest > maximum:
            failures.append(str(library))
    if not checked:
        raise RuntimeError(f"No ELF payloads found in {path}")
    if failures:
        raise RuntimeError(f"glibc {'.'.join(map(str, maximum))} baseline exceeded: {failures}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", type=Path)
    parser.add_argument("--max", default="2.31")
    parser.add_argument("--readelf", default="readelf")
    args = parser.parse_args()
    audit(args.path, tuple(map(int, args.max.split("."))), args.readelf)
