#!/usr/bin/env python3
"""Stage the unmodified LiteParse PDFium SDK selected by its Cargo build script."""
import argparse
import os
from pathlib import Path
import shutil
import tempfile

LIBRARIES = {"macos": "libpdfium.dylib", "windows": "pdfium.dll", "linux": "libpdfium.so"}


def atomic_copy(source, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=destination.parent, delete=False) as out:
        temporary = Path(out.name)
    try:
        shutil.copy2(source, temporary)
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)


def license_text(path):
    data = path.read_bytes()
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError:
        return data.decode("latin-1")


def prepare(source_lib, target, profile):
    platform = "macos" if "apple" in target else "windows" if "windows" in target else "linux"
    name = LIBRARIES[platform]
    sdk = source_lib.parent
    source = (sdk / "bin" if platform == "windows" else source_lib) / name
    version = (sdk / "VERSION").read_text(encoding="utf-8")
    if "BUILD=8058" not in version.splitlines():
        raise RuntimeError("LiteParse requires its patched PDFium chromium/8058 SDK")
    if not source.is_file():
        raise RuntimeError(f"Missing LiteParse PDFium runtime: {source}")
    notices = [sdk / "LICENSE", *sorted((sdk / "licenses").glob("*"))]
    if len(notices) < 2 or not all(p.is_file() for p in notices):
        raise RuntimeError("Missing PDFium redistribution notices")
    atomic_copy(source, profile / name)
    destination = profile / "licenses/pdfium-ThirdPartyNotices.txt"
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text("\n\n".join(p.name + "\n" + license_text(p) for p in notices), encoding="utf-8")
    return profile / name


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-lib", type=Path, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--cargo-out-dir", type=Path, required=True)
    args = parser.parse_args()
    prepare(args.source_lib.resolve(), args.target, args.cargo_out_dir.resolve().parents[2])


if __name__ == "__main__":
    main()
