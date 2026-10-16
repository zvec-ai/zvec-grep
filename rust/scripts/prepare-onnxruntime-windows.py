#!/usr/bin/env python3
"""Download the locked Windows runtime before ort-sys starts its build."""

import argparse
import csv
import hashlib
import json
import lzma
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile


TARGET = "x86_64-pc-windows-msvc"
TARGETS = (TARGET, "aarch64-pc-windows-msvc")


def distribution(workspace, target=TARGET):
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1",
         "--filter-platform", target],
        cwd=workspace, encoding="utf-8",
    ))
    package = next(p for p in metadata["packages"] if p["name"] == "ort-sys")
    manifest = Path(package["manifest_path"]).parent / "build/download/dist.tsv"
    with manifest.open(newline="", encoding="utf-8") as source:
        matches = [row for row in csv.DictReader(source, delimiter="\t")
                   if row["target"] == target and row["feature_set"] == "directml"]
    if len(matches) != 1:
        raise RuntimeError("locked ort-sys must provide one Windows DirectML distribution")
    return matches[0]


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def extract_verified(archive, destination, expected):
    if digest(archive) != expected:
        raise RuntimeError("ONNX Runtime archive SHA-256 differs from locked ort-sys")
    destination.parent.mkdir(parents=True, exist_ok=True)
    # Seed ort-sys's own cache without changing its static-link feature selection.
    # Its archive is a raw LZMA2 stream with a 64 MiB dictionary, followed by tar.
    with tempfile.TemporaryDirectory(dir=destination.parent) as temporary:
        with lzma.open(archive, format=lzma.FORMAT_RAW,
                       filters=[{"id": lzma.FILTER_LZMA2, "dict_size": 1 << 26}]) as stream:
            with tarfile.open(fileobj=stream, mode="r|") as tar:
                tar.extractall(temporary, filter="data")
        for name in ("onnxruntime.lib", "DirectML.dll"):
            if not (Path(temporary) / name).is_file():
                raise RuntimeError(f"ONNX Runtime archive is missing {name}")
        (Path(temporary) / ".verified-sha256").write_text(expected, encoding="ascii")
        if destination.exists():
            shutil.rmtree(destination)
        Path(temporary).rename(destination)


def prepare(workspace, cache, target=TARGET):
    dist = distribution(workspace, target)
    expected = dist["sha256_hash"]
    if len(expected) != 64 or any(c not in "0123456789abcdef" for c in expected):
        raise RuntimeError("locked ort-sys has an invalid distribution checksum")
    cache.mkdir(parents=True, exist_ok=True)
    archive = cache / f"{expected}.tar.lzma2"
    if not archive.is_file() or digest(archive) != expected:
        archive.unlink(missing_ok=True)
        # Bound slow or interrupted transfers and retry before compiling Rust.
        subprocess.run([
            "curl.exe" if os.name == "nt" else "curl",
            "--fail", "--location", "--retry", "3", "--retry-all-errors",
            "--retry-delay", "2", "--connect-timeout", "20", "--max-time", "180",
            "--speed-limit", "1024", "--speed-time", "30",
            "--output", str(archive), dist["url"],
        ], check=True)
    if digest(archive) != expected:
        raise RuntimeError("ONNX Runtime archive SHA-256 differs from locked ort-sys")
    destination = cache / "dfbin" / target / expected
    marker = destination / ".verified-sha256"
    if (not marker.is_file() or marker.read_text(encoding="ascii") != expected
            or not all((destination / name).is_file()
                       for name in ("onnxruntime.lib", "DirectML.dll"))):
        extract_verified(archive, destination, expected)
    print(f"Verified Windows ONNX Runtime cache: {destination}")
    if output := os.environ.get("GITHUB_ENV"):
        with open(output, "a", encoding="utf-8") as environment:
            environment.write(f"ORT_CACHE_DIR={cache}\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache-dir", type=Path, default=Path("target/ort-prebuilt"))
    parser.add_argument("--target", choices=TARGETS, default=TARGET)
    args = parser.parse_args()
    prepare(Path(__file__).resolve().parent.parent, args.cache_dir.resolve(), args.target)
