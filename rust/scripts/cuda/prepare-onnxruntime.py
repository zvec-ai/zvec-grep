#!/usr/bin/env python3
"""Stage the official, hash-pinned ORT 1.28 CUDA 12 runtime (no source patches)."""

import argparse
import hashlib
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tarfile
import tempfile
import zipfile

VERSION = "1.28.0"
DISTRIBUTIONS = {
    "linux": ("onnxruntime-linux-x64-gpu_cuda12-1.28.0.tgz",
              "ea6bd2b65d7dfabbeb92c4af5dd8f12e5aed8601e544ad378d2f872275438b1a"),
    "win32": ("onnxruntime-win-x64-gpu_cuda12-1.28.0.zip",
              "6b7bf16d6d30180db7f386fb179aa4e4f1313f0924531a2879b7b090b56518c1"),
}


def digest(path):
    sha = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            sha.update(block)
    return sha.hexdigest()


def required_files(platform):
    if platform == "win32":
        return {"onnxruntime.dll": "onnxruntime.dll",
                "onnxruntime_providers_shared.dll": "onnxruntime_providers_shared.dll",
                "onnxruntime_providers_cuda.dll": "onnxruntime_providers_cuda.dll"}
    return {f"libonnxruntime.so.{VERSION}": "libonnxruntime.so.1",
            "libonnxruntime_providers_shared.so": "libonnxruntime_providers_shared.so",
            "libonnxruntime_providers_cuda.so": "libonnxruntime_providers_cuda.so"}


def extract_runtime(archive, destination, platform, expected):
    if digest(archive) != expected:
        raise RuntimeError("ONNX Runtime archive SHA-256 mismatch")
    wanted = {**required_files(platform), "LICENSE": "licenses/onnxruntime-LICENSE",
              "ThirdPartyNotices.txt": "licenses/onnxruntime-ThirdPartyNotices.txt"}
    destination.parent.mkdir(parents=True, exist_ok=True)
    # Read only exact regular-file payloads. Never extract archive paths or links.
    with tempfile.TemporaryDirectory(dir=destination.parent) as temporary:
        staged = Path(temporary)
        found = set()
        def copy(name, stream):
            basename = name.rsplit("/", 1)[-1]
            if basename not in wanted:
                return
            if basename in found:
                raise RuntimeError(f"Duplicate runtime file: {basename}")
            output_path = staged / wanted[basename]
            output_path.parent.mkdir(parents=True, exist_ok=True)
            with output_path.open("wb") as output:
                shutil.copyfileobj(stream, output)
            found.add(basename)
        if platform == "win32":
            with zipfile.ZipFile(archive) as package:
                for entry in package.infolist():
                    if (not entry.is_dir() and not stat.S_ISLNK(entry.external_attr >> 16)
                            and entry.filename.rsplit("/", 1)[-1] in wanted):
                        with package.open(entry) as stream:
                            copy(entry.filename, stream)
        else:
            with tarfile.open(archive, "r:gz") as package:
                for entry in package:
                    if entry.isfile() and entry.name.rsplit("/", 1)[-1] in wanted:
                        with package.extractfile(entry) as stream:
                            copy(entry.name, stream)
        if found != set(wanted):
            raise RuntimeError(f"ORT archive missing files: {set(wanted) - found}")
        destination.mkdir(parents=True, exist_ok=True)
        for name in wanted.values():
            (destination / name).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(staged / name, destination / name)


def prepare(cache, destination, platform):
    name, checksum = DISTRIBUTIONS[platform]
    cache.mkdir(parents=True, exist_ok=True)
    archive = cache / name
    if not archive.is_file() or digest(archive) != checksum:
        partial = archive.with_suffix(archive.suffix + ".partial")
        subprocess.run(["curl.exe" if os.name == "nt" else "curl", "-fL",
                        "--retry", "3", "--retry-all-errors", "--connect-timeout", "20",
                        "--max-time", "600", "-o", str(partial),
                        f"https://github.com/microsoft/onnxruntime/releases/download/v{VERSION}/{name}"], check=True)
        if digest(partial) != checksum:
            raise RuntimeError("Downloaded ORT archive SHA-256 mismatch")
        partial.replace(archive)
    extract_runtime(archive, destination, platform, checksum)
    print(f"Verified ORT {VERSION} CUDA 12 runtime: {destination}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", choices=DISTRIBUTIONS, required=True)
    parser.add_argument("--cache-dir", type=Path, required=True)
    parser.add_argument("--destination", type=Path, required=True)
    args = parser.parse_args()
    prepare(args.cache_dir.resolve(), args.destination.resolve(), args.platform)
