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

from .ci import secret_values
from .config import BENCHMARK_ROOT, DEFAULT_ARTIFACTS_DIR, load_config
from .runtime_snapshot import (
    FORMAT,
    MANIFEST_PATH,
    STATE_FILES,
    allowed_path,
    identity,
    inputs,
    restore_baseline,
    validate_manifest,
)

# Frozen preparation sources from 42102cd9. This permits one-time conversion of
# only the exact prior dataset/index/candidate/root identity, without reindexing.
LEGACY_SOURCES = {
    "zg_bench/ci_snapshot.py": "848418436c4ab339d07353b5089d135cceb55c3ae1fc58584652c144b18c92b0",
    "zg_bench/cli.py": "e0ea0b4b4033750b36e60dd06711afe53b8a829dfa8bc39631ece5443048d699",
    "zg_bench/dataset.py": "6c595bd1edd862da64cef145ad3c0fd9fdd833754028caadf3324f518a5bb50f",
    "zg_bench/corpus.py": "c0541ee452fba74f01ddc84add08476a7a30f06532d4a7f33d1a0e9a60b1a194",
    "zg_bench/index.py": "8f1abc5adfc3c1daf4e7ce6d685d31421db84b7ec8766008711e9db2eebdd1ff",
    "zg_bench/profiles.py": "e894a74936555458dde7e647d53524fb1eb079f8ea0b8a6dcefe17ecec7b65f1",
    "zg_bench/artifacts.py": "a7ee4713f638764da869ef88026884ba5f4a956e0bf084c5c8fce2bfef9ef11d",
    "zg_bench/config.py": "2396a33462c9245a6b1c0bebee82d344708bb59556ae122bcc8b9f703dbe1e71",
    "zg_bench/process.py": "372a0ba92a3fd6e64989e7372a0fa15f98d5d316b83cb938b0c2830deaa36ae2",
    "uv.lock": "6198b60421fefe0053977270449438beda136336c8281212ada323325dabfe7b",
}


def cache_key(
    candidate: str, endpoint: str, artifacts: Path, *, legacy: bool = False
) -> str:
    config = load_config()
    schema = 2 if legacy else 3
    sources = (
        LEGACY_SOURCES
        if legacy
        else {
            name: hashlib.sha256((BENCHMARK_ROOT / name).read_bytes()).hexdigest()
            for name in (*LEGACY_SOURCES, "zg_bench/runtime_snapshot.py")
        }
    )
    description = {
        "schema": schema,
        "candidate": candidate,
        "endpoint": None
        if config.zvec_grep.embedding.startswith("local/")
        else endpoint,
        "root": str(artifacts.resolve()),
        "dataset": asdict(config.dataset),
        "index": asdict(config.zvec_grep),
        "sources": sources,
    }
    return (
        f"browse-comp-prepared-v{schema}-"
        + hashlib.sha256(json.dumps(description, sort_keys=True).encode()).hexdigest()
    )


def _allowed(path: PurePosixPath) -> bool:
    if path.is_absolute() or ".." in path.parts or not path.parts:
        return False
    return path.parts[0] in {"source", "workspaces", "models"} or (
        len(path.parts) == 2 and path.parts[0] == "state" and path.name in STATE_FILES
    )


class _CheckedReader:
    def __init__(self, source, secrets: set[bytes]):
        self.source = source
        self.secrets = secrets
        self.digest = hashlib.sha256()
        self.overlap = max((len(value) for value in secrets), default=1) - 1
        self.tail = b""

    def read(self, size=-1):
        data = self.source.read(size)
        window = self.tail + data
        if any(value in window for value in self.secrets):
            raise ValueError("unsafe or credential-bearing prepared snapshot file")
        self.tail = window[-self.overlap :] if self.overlap else b""
        self.digest.update(data)
        return data


def _add_json(archive, name: str, value: dict) -> None:
    import io

    encoded = json.dumps(value, sort_keys=True).encode()
    info = tarfile.TarInfo(name)
    info.size = len(encoded)
    info.pax_headers["mtime"] = "0.000000000"
    archive.addfile(info, io.BytesIO(encoded))


def write_tar(artifacts: Path, destination, key: str, secrets: set[bytes]) -> None:
    config = load_config()
    paths, baseline = inputs(config, artifacts)
    manifest = {**identity(config, artifacts, key), "baseline": baseline, "files": []}
    with tarfile.open(
        fileobj=destination, mode="w|", format=tarfile.PAX_FORMAT
    ) as archive:
        _add_json(
            archive,
            "snapshot.json",
            {
                "key": key,
                "root": str(artifacts.resolve()),
                "format": FORMAT,
            },
        )
        for path in paths:
            if path.is_symlink():
                raise ValueError("prepared snapshots cannot contain symlinks")
            if not path.is_file() or path.name == "auth.json":
                raise ValueError("unsafe or credential-bearing prepared snapshot file")
            relative = path.relative_to(artifacts).as_posix()
            if not allowed_path(PurePosixPath(relative)):
                raise ValueError("unexpected prepared snapshot path")
            info = archive.gettarinfo(str(path), arcname=relative)
            nanoseconds = path.stat().st_mtime_ns
            info.pax_headers["mtime"] = (
                f"{nanoseconds // 10**9}.{nanoseconds % 10**9:09d}"
            )
            with path.open("rb") as source:
                reader = _CheckedReader(source, secrets)
                archive.addfile(info, reader)
            manifest["files"].append(
                {
                    "path": relative,
                    "bytes": info.size,
                    "sha256": reader.digest.hexdigest(),
                    "mtime_ns": nanoseconds,
                    "mode": info.mode & 0o777,
                }
            )
        validate_manifest(manifest, config, artifacts, key)
        _add_json(archive, MANIFEST_PATH, manifest)


def read_tar(artifacts: Path, source, key: str) -> None:
    if artifacts.exists() and any(artifacts.iterdir()):
        raise ValueError("restore requires an empty artifacts directory")
    artifacts.mkdir(parents=True, exist_ok=True)
    seen = set()
    records = []
    runtime = False
    with tarfile.open(fileobj=source, mode="r|") as archive:
        header = archive.next()
        if (
            header is None
            or header.name != "snapshot.json"
            or not header.isfile()
            or header.size > 65536
        ):
            raise ValueError("prepared snapshot header is missing or invalid")
        contents = archive.extractfile(header)
        assert contents is not None
        metadata = json.load(contents)
        expected = {"key": key, "root": str(artifacts.resolve())}
        runtime = metadata == {**expected, "format": FORMAT}
        if not runtime and (
            metadata != expected or key.startswith("browse-comp-prepared-v3-")
        ):
            raise ValueError("prepared snapshot identity mismatch")
        for member in archive:
            if member is header:
                continue
            relative = PurePosixPath(member.name)
            if (
                not (allowed_path(relative) if runtime else _allowed(relative))
                or relative.as_posix() != member.name
                or not member.isfile()
                or member.name in seen
                or MANIFEST_PATH in seen
            ):
                raise ValueError("unsafe or duplicate prepared snapshot entry")
            seen.add(member.name)
            target = artifacts / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            stream = archive.extractfile(member)
            assert stream is not None
            reader = _CheckedReader(stream, set())
            with target.open("xb") as output:
                shutil.copyfileobj(reader, output)
            target.chmod(member.mode & 0o777)
            # Preserve the exact corpus timestamps stored in the ready index.
            try:
                nanoseconds = int(Decimal(member.pax_headers["mtime"]) * 10**9)
            except (KeyError, ValueError, ArithmeticError) as error:
                raise ValueError("prepared snapshot timestamp is invalid") from error
            os.utime(target, ns=(nanoseconds, nanoseconds))
            if member.name != MANIFEST_PATH:
                records.append(
                    {
                        "path": member.name,
                        "bytes": member.size,
                        "sha256": reader.digest.hexdigest(),
                        "mtime_ns": nanoseconds,
                        "mode": member.mode & 0o777,
                    }
                )
    if runtime:
        if MANIFEST_PATH not in seen:
            raise ValueError("runtime snapshot is incomplete")
        manifest = json.loads((artifacts / MANIFEST_PATH).read_text())
        validate_manifest(manifest, load_config(), artifacts, key)
        if manifest["files"] != records:
            raise ValueError("runtime snapshot contents do not match the file manifest")
        restore_baseline(manifest, artifacts)
    elif not all(f"state/{name}" in seen for name in STATE_FILES) or (
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
        for name, legacy in (("key", False), ("legacy_key", True)):
            print(
                name
                + "="
                + cache_key(
                    os.environ["BROWSE_CANDIDATE_COMMIT"],
                    os.environ.get("ZVEC_GREP_ENDPOINT", ""),
                    DEFAULT_ARTIFACTS_DIR,
                    legacy=legacy,
                )
            )
        return 0
    if not args.archive or not args.key:
        parser.error("pack/unpack require --archive and --key")
    if args.command == "pack":
        verify_snapshot(DEFAULT_ARTIFACTS_DIR)
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
