#!/usr/bin/env python3
"""Stage the official, hash-pinned ORT 1.28 CUDA 12 runtime (no source patches)."""

import argparse
import calendar
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


def copy_runtime(source, destination):
    """Independent, atomic copies; leave identical output mtimes untouched."""
    destination.parent.mkdir(parents=True, exist_ok=True)
    if (not destination.is_symlink() and destination.is_file()
            and digest(destination) == digest(source)):
        return
    with tempfile.NamedTemporaryFile(dir=destination.parent, delete=False) as output:
        temporary = Path(output.name)
    try:
        shutil.copy2(source, temporary)
        temporary.replace(destination)
    finally:
        temporary.unlink(missing_ok=True)


def extract_runtime(archive, destination, platform, expected, *, cargo=False):
    if digest(archive) != expected:
        raise RuntimeError("ONNX Runtime archive SHA-256 mismatch")
    wanted = {**required_files(platform), "LICENSE": "licenses/onnxruntime-LICENSE",
              "ThirdPartyNotices.txt": "licenses/onnxruntime-ThirdPartyNotices.txt"}
    destination.parent.mkdir(parents=True, exist_ok=True)
    # Read only exact regular-file payloads. Never extract archive paths or links.
    with tempfile.TemporaryDirectory(dir=destination.parent) as temporary:
        staged = Path(temporary)
        found = set()
        def copy(name, stream, mtime):
            basename = name.rsplit("/", 1)[-1]
            if basename not in wanted:
                return
            if basename in found:
                raise RuntimeError(f"Duplicate runtime file: {basename}")
            output_path = staged / wanted[basename]
            output_path.parent.mkdir(parents=True, exist_ok=True)
            with output_path.open("wb") as output:
                shutil.copyfileobj(stream, output)
            # Cargo watches the deployed files to repair deleted outputs. Use
            # the archive timestamp, not the extraction time, so the next build
            # remains fresh rather than seeing its own outputs as new inputs.
            os.utime(output_path, (mtime, mtime))
            found.add(basename)
        if platform == "win32":
            with zipfile.ZipFile(archive) as package:
                for entry in package.infolist():
                    if (not entry.is_dir() and not stat.S_ISLNK(entry.external_attr >> 16)
                            and entry.filename.rsplit("/", 1)[-1] in wanted):
                        with package.open(entry) as stream:
                            copy(entry.filename, stream, calendar.timegm((*entry.date_time, 0, 0, 0)))
        else:
            with tarfile.open(archive, "r:gz") as package:
                for entry in package:
                    if entry.isfile() and entry.name.rsplit("/", 1)[-1] in wanted:
                        with package.extractfile(entry) as stream:
                            copy(entry.name, stream, entry.mtime)
        if found != set(wanted):
            raise RuntimeError(f"ORT archive missing files: {set(wanted) - found}")
        directories = [destination]
        if cargo:
            directories += [destination / "deps", destination / "examples"]
        outputs = []
        for directory in directories:
            for name in wanted.values():
                path = directory / name
                copy_runtime(staged / name, path)
                outputs.append(path)
        return outputs


def prepare(cache, destination, platform, *, cargo=False, offline=False):
    name, checksum = DISTRIBUTIONS[platform]
    cache.mkdir(parents=True, exist_ok=True)
    archive = cache / name
    if not archive.is_file() or digest(archive) != checksum:
        if offline:
            raise RuntimeError(f"Offline build requires the verified ORT archive: {archive}")
        # Different profiles/workspaces may populate this cache concurrently.
        # Never share a partial download or expose an unverified final archive.
        with tempfile.TemporaryDirectory(dir=cache) as temporary:
            partial = Path(temporary) / name
            subprocess.run(["curl.exe" if os.name == "nt" else "curl", "-fL",
                            "--retry", "3", "--retry-all-errors", "--connect-timeout", "20",
                            "--max-time", "600", "-o", str(partial),
                            f"https://github.com/microsoft/onnxruntime/releases/download/v{VERSION}/{name}"], check=True)
            if digest(partial) != checksum:
                raise RuntimeError("Downloaded ORT archive SHA-256 mismatch")
            partial.replace(archive)
    outputs = extract_runtime(archive, destination, platform, checksum, cargo=cargo)
    print(f"Verified ORT {VERSION} CUDA 12 runtime: {destination}")
    if cargo:
        for path in outputs:
            print(f"cargo:rerun-if-changed={path}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", choices=DISTRIBUTIONS, required=True)
    parser.add_argument("--cache-dir", type=Path)
    layout = parser.add_mutually_exclusive_group(required=True)
    layout.add_argument("--destination", type=Path)
    layout.add_argument("--cargo-out-dir", type=Path,
                        help="Stage beside Cargo binaries, tests and examples; share the npm ORT cache")
    args = parser.parse_args()
    if args.cargo_out_dir:
        # <target>[/<triple>]/<profile>/build/<package-hash>/out
        profile = args.cargo_out_dir.resolve().parents[2]
        if args.cache_dir:
            parser.error("--cache-dir cannot be combined with --cargo-out-dir")
        prepare(profile.parent / "ort-cuda12", profile, args.platform, cargo=True,
                offline=os.environ.get("CARGO_NET_OFFLINE", "").lower() in ("true", "1"))
    else:
        if not args.cache_dir:
            parser.error("--destination requires --cache-dir")
        prepare(args.cache_dir.resolve(), args.destination.resolve(), args.platform)
