#!/usr/bin/env python3
"""Deploy the pinned official ORT runtime for Cargo and npm, without compiling it."""

import argparse
import calendar
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import stat
import subprocess
import tarfile
import tempfile
import zipfile

MANIFEST_PATH = Path(__file__).with_name("onnxruntime-distributions.json")
MARKER = ".ort-runtime.json"


def digest(path):
    sha = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            sha.update(block)
    return sha.hexdigest()


def relative_path(value):
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts or "\\" in value or ":" in value:
        raise RuntimeError(f"Unsafe runtime path: {value}")
    return path


def selection(manifest, target, cuda=False):
    variant = "cuda" if cuda else "cpu"
    try:
        return [manifest["artifacts"][name] for name in manifest["targets"][target][variant]]
    except KeyError as error:
        raise RuntimeError(f"No official ORT {variant} distribution for {target}") from error


def cached_archive(cache, artifact, offline):
    archive = cache / artifact["archive"]
    expected = artifact["sha256"]
    if archive.is_file() and digest(archive) == expected:
        return archive
    if offline:
        raise RuntimeError(f"Offline build requires the verified ORT archive: {archive}")
    cache.mkdir(parents=True, exist_ok=True)
    # Concurrent builds never share partial downloads or expose unverified bytes.
    with tempfile.TemporaryDirectory(dir=cache) as temporary:
        partial = Path(temporary) / "download"
        subprocess.run(["curl.exe" if os.name == "nt" else "curl", "-fL",
                        "--retry", "3", "--retry-all-errors", "--connect-timeout", "20",
                        "--max-time", "600", "-o", str(partial), artifact["url"]], check=True)
        if digest(partial) != expected:
            raise RuntimeError(f"Downloaded ORT archive SHA-256 mismatch: {artifact['url']}")
        partial.replace(archive)
    return archive


def extract_runtime(archive, staged, artifact):
    if digest(archive) != artifact["sha256"]:
        raise RuntimeError("ONNX Runtime archive SHA-256 mismatch")
    wanted = artifact["files"]
    for source, destination in wanted.items():
        relative_path(source)
        relative_path(destination)
    found = set()

    def copy(name, stream, mtime):
        if name in found:
            raise RuntimeError(f"Duplicate runtime file: {name}")
        destination = staged / wanted[name]
        if destination.exists():
            raise RuntimeError(f"Conflicting runtime destination: {wanted[name]}")
        destination.parent.mkdir(parents=True, exist_ok=True)
        with destination.open("wb") as output:
            shutil.copyfileobj(stream, output)
        os.utime(destination, (mtime, mtime))
        found.add(name)

    # Match full archive paths, never basenames: macOS archives also contain
    # dSYM objects named exactly like the actual dylib. Never extract links.
    if zipfile.is_zipfile(archive):
        with zipfile.ZipFile(archive) as package:
            for entry in package.infolist():
                if (entry.filename in wanted and not entry.is_dir()
                        and not stat.S_ISLNK(entry.external_attr >> 16)):
                    with package.open(entry) as stream:
                        copy(entry.filename, stream, calendar.timegm((*entry.date_time, 0, 0, 0)))
    else:
        with tarfile.open(archive, "r:gz") as package:
            for entry in package:
                if entry.isfile() and entry.name in wanted:
                    with package.extractfile(entry) as stream:
                        copy(entry.name, stream, entry.mtime)
    if found != set(wanted):
        raise RuntimeError(f"ORT archive missing files: {set(wanted) - found}")


def copy_runtime(source, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    if not destination.is_symlink() and destination.is_file() and digest(destination) == digest(source):
        return
    with tempfile.NamedTemporaryFile(dir=destination.parent, delete=False) as output:
        temporary = Path(output.name)
    try:
        shutil.copy2(source, temporary)
        temporary.replace(destination)
    finally:
        temporary.unlink(missing_ok=True)


def deploy(staged, destination, files, identity):
    marker = destination / MARKER
    old = json.loads(marker.read_text()) if marker.is_file() else {}
    # Remove only files owned by our previous deployment when switching variants.
    # A failed download/extraction never reaches deployment.
    for name in set(old.get("files", {})) - set(files):
        relative_path(name)
        path = destination / name
        if path.is_file() or path.is_symlink():
            if path.is_symlink() or digest(path) != old["files"][name]:
                raise RuntimeError(f"Previously deployed ORT file was modified: {path}")
    for name in files:
        copy_runtime(staged / name, destination / name)
    for name in set(old.get("files", {})) - set(files):
        (destination / name).unlink(missing_ok=True)
    state = {**identity, "files": files}
    if state != old:
        with tempfile.NamedTemporaryFile(mode="w", dir=destination, delete=False) as output:
            json.dump(state, output, indent=2)
            output.write("\n")
            temporary = Path(output.name)
        temporary.replace(marker)
    return [destination / name for name in files] + [marker]


def prepare(cache, destination, target, *, cuda=False, cargo=False, offline=False, manifest=None):
    manifest = manifest or json.loads(MANIFEST_PATH.read_text())
    artifacts = selection(manifest, target, cuda)
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=destination.parent) as temporary:
        staged = Path(temporary)
        for artifact in artifacts:
            extract_runtime(cached_archive(cache, artifact, offline), staged, artifact)
        files = {p.relative_to(staged).as_posix(): digest(p) for p in staged.rglob("*") if p.is_file()}
        identity = {"version": manifest["version"], "target": target, "variant": "cuda" if cuda else "cpu"}
        directories = [destination]
        if cargo:
            directories += [destination / "deps", destination / "examples"]
        outputs = []
        for directory in directories:
            directory.mkdir(parents=True, exist_ok=True)
            outputs += deploy(staged, directory, files, identity)
    print(f"Verified ORT {manifest['version']} {target} {identity['variant']}: {destination}")
    if cargo:
        for path in outputs:
            print(f"cargo:rerun-if-changed={path}")
    return outputs


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--cuda", action="store_true")
    parser.add_argument("--cache-dir", type=Path)
    parser.add_argument("--offline", action="store_true")
    layout = parser.add_mutually_exclusive_group(required=True)
    layout.add_argument("--destination", type=Path)
    layout.add_argument("--cargo-out-dir", type=Path)
    args = parser.parse_args()
    if args.cargo_out_dir:
        # <target>[/<triple>]/<profile>/build/<package-hash>/out
        destination = args.cargo_out_dir.resolve().parents[2]
        cache = args.cache_dir or destination.parent / "ort-prebuilt"
    else:
        destination = args.destination.resolve()
        if not args.cache_dir:
            parser.error("--destination requires --cache-dir")
        cache = args.cache_dir
    prepare(cache.resolve(), destination, args.target, cuda=args.cuda, cargo=bool(args.cargo_out_dir),
            offline=args.offline or os.environ.get("CARGO_NET_OFFLINE", "").lower() in ("true", "1"))


if __name__ == "__main__":
    main()
