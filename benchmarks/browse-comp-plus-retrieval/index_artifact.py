"""Seal and safely restore a complete Rust BrowseComp index; never execute contents."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import tarfile
import zipfile
from pathlib import Path, PurePosixPath


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as source:
        for part in iter(lambda: source.read(1024 * 1024), b""):
            value.update(part)
    return value.hexdigest()


def safe_name(name: str) -> str:
    path = PurePosixPath(name)
    if path.is_absolute() or ".." in path.parts or "\\" in name:
        raise ValueError(f"unsafe artifact member: {name}")
    if not path.parts or path.parts[0] not in {"index", "model", "build"}:
        raise ValueError(f"unexpected artifact member: {name}")
    return str(path)


def files(root: Path, prefix: str) -> list[tuple[str, Path]]:
    result = []
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        if prefix == "build" and "runtime-home" in relative.parts:
            continue
        if path.is_symlink():
            raise ValueError("index artifacts cannot contain symlinks")
        if path.is_file():
            result.append((f"{prefix}/{relative.as_posix()}", path))
        elif not path.is_dir():
            raise ValueError("unsupported artifact file type")
    return result


def validate_manifest(value: dict, key: str) -> None:
    if not re.fullmatch(r"[a-f0-9]{64}", key):
        raise ValueError("invalid storage key")
    if value.get("schema_version") != 1 or value.get("key") != key:
        raise ValueError("index artifact identity mismatch")
    build = value["build"]
    if build.get("status") != "success" or build["document_count"] != 100195:
        raise ValueError("artifact does not contain a complete full-corpus build")
    if build["identity"]["key"] != key:
        raise ValueError("builder storage identity mismatch")
    index = build["arm"]["performance_groups"][0]["index"]
    if index["status"] != "success" or index["memory"]["status"] != "available":
        raise ValueError("artifact has incomplete Index measurements")
    if not isinstance(index.get("index_bytes"), int) or index["index_bytes"] <= 0:
        raise ValueError("missing index size")
    if build["status_snapshot"]["files"] != 100195:
        raise ValueError("incomplete public index coverage")
    names = [safe_name(row["path"]) for row in value["files"]]
    if len(set(names)) != len(names) or not any(n.startswith("index/") for n in names):
        raise ValueError("duplicate or missing index members")
    if not any(n.startswith("model/") for n in names):
        raise ValueError("missing pinned model bytes")


def pack(index: Path, model: Path, evidence: Path, report: Path, output: Path) -> dict:
    build = json.loads(report.read_text())
    output.mkdir(parents=True, exist_ok=False)
    members = files(index, "index") + files(model, "model") + files(evidence, "build")
    value = {
        "schema_version": 1,
        "kind": "browsecomp-doc-index",
        "key": build["identity"]["key"],
        "build": build,
        "files": [
            {"path": name, "bytes": path.stat().st_size, "sha256": digest(path)}
            for name, path in members
        ],
    }
    validate_manifest(value, value["key"])
    if (
        sum(row["bytes"] for row in value["files"] if row["path"].startswith("index/"))
        != build["arm"]["performance_groups"][0]["index"]["index_bytes"]
    ):
        raise ValueError("index size changed after measurement")
    archive = output / "index.tar.gz"
    with tarfile.open(archive, "w:gz", compresslevel=1) as target:
        for name, path in members:
            target.add(path, arcname=name, recursive=False)
    value["archive_sha256"] = digest(archive)
    value["archive_bytes"] = archive.stat().st_size
    (output / "manifest.json").write_text(json.dumps(value, indent=2) + "\n")
    verify(output, value["key"])
    return value


def verify(directory: Path, key: str, output: Path | None = None) -> dict:
    value = json.loads((directory / "manifest.json").read_text())
    validate_manifest(value, key)
    archive = directory / "index.tar.gz"
    if digest(archive) != value["archive_sha256"]:
        raise ValueError("index archive checksum mismatch")
    expected = {row["path"]: row for row in value["files"]}
    seen = set()
    if output is not None:
        output.mkdir(parents=True, exist_ok=False)
    with tarfile.open(archive, "r:gz") as source:
        for member in source:
            name = safe_name(member.name)
            if not member.isfile() or name in seen or name not in expected:
                raise ValueError("unexpected, duplicate or linked archive member")
            seen.add(name)
            row = expected[name]
            if member.size != row["bytes"]:
                raise ValueError("index member size mismatch")
            stream = source.extractfile(member)
            assert stream is not None
            checksum = hashlib.sha256()
            destination = None
            if output is not None:
                path = output / name
                path.parent.mkdir(parents=True, exist_ok=True)
                destination = path.open("xb")
            try:
                for part in iter(lambda source=stream: source.read(1024 * 1024), b""):
                    checksum.update(part)
                    if destination:
                        destination.write(part)
            finally:
                if destination:
                    destination.close()
            if checksum.hexdigest() != row["sha256"]:
                raise ValueError("index member checksum mismatch")
    if seen != set(expected):
        raise ValueError("index archive members are missing")
    return value


def unzip(archive: Path, output: Path) -> None:
    output.mkdir(parents=True, exist_ok=False)
    with zipfile.ZipFile(archive) as source:
        entries = source.infolist()
        if {row.filename for row in entries} != {
            "index.tar.gz",
            "manifest.json",
        } or len(entries) != 2:
            raise ValueError("unexpected index artifact ZIP members")
        for row in entries:
            if (row.external_attr >> 16) & 0o170000 == 0o120000:
                raise ValueError("artifact ZIP contains a symlink")
            with (
                source.open(row) as stream,
                (output / row.filename).open("xb") as target,
            ):
                shutil.copyfileobj(stream, target)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["pack", "verify", "extract", "unzip"])
    for option in [
        "index",
        "model",
        "evidence",
        "report",
        "output",
        "directory",
        "archive",
    ]:
        parser.add_argument(f"--{option}", type=Path)
    parser.add_argument("--key")
    args = parser.parse_args()
    if args.command == "pack":
        pack(args.index, args.model, args.evidence, args.report, args.output)
    elif args.command in {"verify", "extract"}:
        verify(
            args.directory, args.key, args.output if args.command == "extract" else None
        )
    else:
        unzip(args.archive, args.output)
