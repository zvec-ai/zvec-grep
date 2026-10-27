"""Portable, credential-free preparation snapshots for CI workers."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import tarfile
from dataclasses import asdict
from decimal import Decimal
from pathlib import Path, PurePosixPath

from .ci import _contains_secret, secret_values
from .config import BENCHMARK_ROOT, DEFAULT_ARTIFACTS_DIR, load_config

STATE_FILES = {"dataset.json", "corpus.json", "corpus-manifest.jsonl", "index.json"}


def cache_key(candidate: str, endpoint: str, artifacts: Path) -> str:
    config = load_config()
    identity = {
        "schema": 2,
        "candidate": candidate,
        "endpoint": None
        if config.zvec_grep.embedding.startswith("local/")
        else endpoint,
        "root": str(artifacts.resolve()),
        "dataset": asdict(config.dataset),
        "index": asdict(config.zvec_grep),
        "sources": {
            name: hashlib.sha256((BENCHMARK_ROOT / name).read_bytes()).hexdigest()
            for name in (
                "zg_bench/ci_snapshot.py",
                "zg_bench/cli.py",
                "zg_bench/dataset.py",
                "zg_bench/corpus.py",
                "zg_bench/index.py",
                "zg_bench/profiles.py",
                "zg_bench/artifacts.py",
                "zg_bench/config.py",
                "zg_bench/process.py",
                "uv.lock",
            )
        },
    }
    return (
        "browse-comp-prepared-v2-"
        + hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()
    )


def _allowed(path: PurePosixPath) -> bool:
    if path.is_absolute() or ".." in path.parts or not path.parts:
        return False
    return path.parts[0] in {"source", "workspaces", "models"} or (
        len(path.parts) == 2 and path.parts[0] == "state" and path.name in STATE_FILES
    )


def write_tar(artifacts: Path, destination, key: str, secrets: set[bytes]) -> None:
    import io

    metadata = json.dumps({"key": key, "root": str(artifacts.resolve())}).encode()
    with tarfile.open(
        fileobj=destination, mode="w|", format=tarfile.PAX_FORMAT
    ) as archive:
        info = tarfile.TarInfo("snapshot.json")
        info.size = len(metadata)
        archive.addfile(info, io.BytesIO(metadata))
        roots = [artifacts / "source", artifacts / "workspaces"]
        if load_config().zvec_grep.embedding.startswith("local/"):
            roots.append(artifacts / "models")
        roots.extend(artifacts / "state" / name for name in sorted(STATE_FILES))
        for root in roots:
            if not root.exists():
                raise ValueError("prepared snapshot input is missing")
            for path in [root, *sorted(root.rglob("*"))] if root.is_dir() else [root]:
                if path.is_symlink():
                    raise ValueError("prepared snapshots cannot contain symlinks")
                if path.is_dir():
                    continue
                if (
                    not path.is_file()
                    or path.name == "auth.json"
                    or _contains_secret(path, secrets)
                ):
                    raise ValueError(
                        "unsafe or credential-bearing prepared snapshot file"
                    )
                relative = path.relative_to(artifacts).as_posix()
                if not _allowed(PurePosixPath(relative)):
                    raise ValueError("unexpected prepared snapshot path")
                info = archive.gettarinfo(str(path), arcname=relative)
                nanoseconds = path.stat().st_mtime_ns
                info.pax_headers["mtime"] = (
                    f"{nanoseconds // 10**9}.{nanoseconds % 10**9:09d}"
                )
                with path.open("rb") as source:
                    archive.addfile(info, source)


def read_tar(artifacts: Path, source, key: str) -> None:
    if artifacts.exists() and any(artifacts.iterdir()):
        raise ValueError("restore requires an empty artifacts directory")
    artifacts.mkdir(parents=True, exist_ok=True)
    seen = set()
    with tarfile.open(fileobj=source, mode="r|") as archive:
        header = archive.next()
        if header is None or header.name != "snapshot.json" or not header.isfile():
            raise ValueError("prepared snapshot header is missing")
        contents = archive.extractfile(header)
        assert contents is not None
        metadata = json.load(contents)
        if metadata != {"key": key, "root": str(artifacts.resolve())}:
            raise ValueError("prepared snapshot identity mismatch")
        for member in archive:
            if member is header:
                continue
            relative = PurePosixPath(member.name)
            if not _allowed(relative) or not member.isfile() or member.name in seen:
                raise ValueError("unsafe or duplicate prepared snapshot entry")
            seen.add(member.name)
            target = artifacts / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            stream = archive.extractfile(member)
            assert stream is not None
            with target.open("xb") as output:
                shutil.copyfileobj(stream, output)
            target.chmod(member.mode & 0o777)
            # tarfile's normal extraction converts PAX timestamps to float, losing ns.
            nanoseconds = int(Decimal(member.pax_headers["mtime"]) * 10**9)
            os.utime(target, ns=(nanoseconds, nanoseconds))
    if not all(f"state/{name}" in seen for name in STATE_FILES) or (
        load_config().zvec_grep.embedding.startswith("local/")
        and not any(name.startswith("models/") for name in seen)
    ):
        raise ValueError("prepared snapshot is incomplete")


def verify_snapshot(artifacts: Path) -> None:
    """Reject a stale restore without silently rebuilding and publishing old bytes."""
    from .corpus import prepared_corpus
    from .dataset import prepared_dataset
    from .index import index_is_ready

    config = load_config()
    if not (
        prepared_dataset(config, artifacts)
        and prepared_corpus(config, artifacts)
        and index_is_ready(config, artifacts)
    ):
        raise ValueError(
            "prepared snapshot is stale; remove its Actions cache and retry"
        )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("key", "pack", "unpack", "verify"))
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--key")
    args = parser.parse_args()
    if args.command == "verify":
        verify_snapshot(DEFAULT_ARTIFACTS_DIR)
        return 0
    if args.command == "key":
        print(
            "key="
            + cache_key(
                os.environ["BROWSE_CANDIDATE_COMMIT"],
                os.environ.get("ZVEC_GREP_ENDPOINT", ""),
                DEFAULT_ARTIFACTS_DIR,
            )
        )
        return 0
    if not args.archive or not args.key:
        parser.error("pack/unpack require --archive and --key")
    if args.command == "pack":
        args.archive.parent.mkdir(parents=True, exist_ok=True)
        process = subprocess.Popen(
            ["zstd", "-T0", "-1", "-f", "-o", str(args.archive)], stdin=subprocess.PIPE
        )
        assert process.stdin is not None
        try:
            with process.stdin:
                write_tar(
                    DEFAULT_ARTIFACTS_DIR,
                    process.stdin,
                    args.key,
                    secret_values(Path(os.environ["OPENCODE_HOME"])),
                )
        except BaseException:
            process.kill()
            process.wait()
            args.archive.unlink(missing_ok=True)
            raise
    else:
        process = subprocess.Popen(
            ["zstd", "-dc", str(args.archive)], stdout=subprocess.PIPE
        )
        assert process.stdout is not None
        try:
            with process.stdout:
                read_tar(DEFAULT_ARTIFACTS_DIR, process.stdout, args.key)
        except BaseException:
            process.kill()
            process.wait()
            raise
    if process.wait() != 0:
        raise RuntimeError("prepared snapshot compression/transport failed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
