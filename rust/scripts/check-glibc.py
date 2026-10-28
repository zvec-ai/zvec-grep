#!/usr/bin/env python3
"""Reject mandatory glibc version requirements above an ELF payload's baseline."""

import argparse
import os
from pathlib import Path
import re
import subprocess


def required_versions(symbols):
    versions = set()
    needs = False
    for line in symbols.splitlines():
        if line.startswith("Version ") and "section" in line:
            needs = line.startswith("Version needs section")
        if needs:
            requirement = re.search(r"Name: (GLIBC_\S+)\s+Flags: (.*?)\s+Version:", line)
            if requirement and "WEAK" not in requirement[2].upper().split():
                version = requirement[1][len("GLIBC_"):]
                if not re.fullmatch(r"[0-9]+(?:\.[0-9]+)+", version):
                    raise RuntimeError(f"Cannot establish a glibc baseline for {requirement[1]}")
                versions.add(tuple(map(int, version.split("."))))
        # STB_WEAK does not make a version optional: the loader checks the
        # independent VER_FLG_WEAK flag in .gnu.version_r before relocations.
        # Strong undefined symbols also establish a floor, even if the version
        # requirement was marked weak by a nonstandard producer.
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
        symbols = subprocess.check_output(
            [readelf, "--dyn-syms", "--version-info", "--wide", str(library)],
            text=True, env={**os.environ, "LC_ALL": "C"})
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
