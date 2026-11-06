"""Validated worker inputs, without download sources or duplicate corpus bytes."""

from __future__ import annotations

import os
import shutil
from dataclasses import asdict
from pathlib import Path, PurePosixPath
from typing import Any

from .artifacts import read_json, read_jsonl, sha256_file
from .config import BenchmarkConfig

STATE_FILES = {"dataset.json", "corpus.json", "corpus-manifest.jsonl", "index.json"}
QUERY_PATH = "source/browsecomp_plus_decrypted.jsonl"
MANIFEST_PATH = "state/runtime-snapshot.json"
FORMAT = "browse-comp-runtime-v1"


def allowed_path(path: PurePosixPath) -> bool:
    if path.is_absolute() or ".." in path.parts or not path.parts:
        return False
    return (
        path.as_posix() in {QUERY_PATH, MANIFEST_PATH}
        or (
            len(path.parts) == 2
            and path.parts[0] == "state"
            and path.name in STATE_FILES
        )
        or (len(path.parts) > 1 and path.parts[0] == "models")
        or (
            path.parts[:2] == ("workspaces", "b")
            and (
                (len(path.parts) == 3 and path.suffix == ".md")
                or (len(path.parts) > 3 and path.parts[2] == ".zvec-grep")
            )
        )
    )


def identity(config: BenchmarkConfig, artifacts: Path, key: str) -> dict[str, Any]:
    return {
        "format": FORMAT,
        "key": key,
        "root": str(artifacts.resolve()),
        "dataset": asdict(config.dataset),
        "index": asdict(config.zvec_grep),
    }


def corpus_entries(config: BenchmarkConfig, artifacts: Path) -> list[dict[str, Any]]:
    from .corpus import _safe_docid

    entries = read_jsonl(artifacts / "state/corpus-manifest.jsonl")
    paths = set()
    for entry in entries:
        docid = _safe_docid(entry["docid"])
        if entry["path"] != f"{docid}.md" or entry["path"] in paths:
            raise ValueError("invalid or duplicate runtime corpus path")
        paths.add(entry["path"])
    if len(paths) != config.dataset.expected_corpus_documents:
        raise ValueError("runtime corpus count does not match configuration")
    return entries


def inputs(config: BenchmarkConfig, artifacts: Path) -> tuple[list[Path], list[dict]]:
    """Check both corpora before omitting baseline's duplicate contents."""
    from .corpus import prepared_corpus
    from .dataset import load_queries, prepared_dataset

    if not prepared_dataset(config, artifacts) or not prepared_corpus(
        config, artifacts
    ):
        raise ValueError("prepared snapshot inputs are stale or missing")
    if len(load_queries(artifacts / QUERY_PATH)) != config.dataset.expected_queries:
        raise ValueError("runtime query count does not match configuration")
    paths = [artifacts / QUERY_PATH]
    baseline = []
    for entry in corpus_entries(config, artifacts):
        source = artifacts / "workspaces/b" / entry["path"]
        original = artifacts / "workspaces/a" / entry["path"]
        for path in (source, original):
            if path.is_symlink() or not path.is_file():
                raise ValueError("runtime corpus must contain only regular files")
            if (
                path.stat().st_size != entry["bytes"]
                or sha256_file(path) != entry["sha256"]
            ):
                raise ValueError("runtime corpus content does not match its manifest")
        metadata = original.stat()
        baseline.append(
            {
                "path": entry["path"],
                "mtime_ns": metadata.st_mtime_ns,
                "mode": metadata.st_mode & 0o777,
            }
        )
        paths.append(source)
    roots = [artifacts / "workspaces/b/.zvec-grep"]
    if config.zvec_grep.embedding.startswith("local/"):
        roots.append(artifacts / "models")
    for root in roots:
        if root.is_symlink() or not root.is_dir():
            raise ValueError("prepared snapshot input is missing or a symlink")
        files = []
        for path in sorted(root.rglob("*")):
            if path.is_symlink():
                raise ValueError("prepared snapshots cannot contain symlinks")
            if not path.is_dir():
                files.append(path)
        if not files:
            raise ValueError("prepared snapshot input is missing")
        paths.extend(files)
    paths.extend(artifacts / "state" / name for name in sorted(STATE_FILES))
    return paths, baseline


def validate_manifest(
    manifest: dict[str, Any], config: BenchmarkConfig, artifacts: Path, key: str
) -> dict[str, dict[str, Any]]:
    if not isinstance(manifest, dict) or any(
        manifest.get(name) != value
        for name, value in identity(config, artifacts, key).items()
    ):
        raise ValueError("runtime snapshot identity mismatch")
    files = manifest.get("files")
    if not isinstance(files, list):
        raise TypeError("runtime snapshot file manifest is missing")
    records = {}
    for entry in files:
        if not isinstance(entry, dict) or set(entry) != {
            "path",
            "bytes",
            "sha256",
            "mtime_ns",
            "mode",
        }:
            raise ValueError("invalid runtime snapshot file entry")
        name = entry["path"]
        if (
            not isinstance(name, str)
            or PurePosixPath(name).as_posix() != name
            or not allowed_path(PurePosixPath(name))
            or name == MANIFEST_PATH
            or name in records
            or not isinstance(entry["bytes"], int)
            or entry["bytes"] < 0
            or not isinstance(entry["mtime_ns"], int)
            or not isinstance(entry["mode"], int)
            or not 0 <= entry["mode"] <= 0o777
            or not isinstance(entry["sha256"], str)
            or len(entry["sha256"]) != 64
            or any(c not in "0123456789abcdef" for c in entry["sha256"])
        ):
            raise ValueError("invalid or duplicate runtime snapshot file entry")
        records[name] = entry
    required = {QUERY_PATH, *(f"state/{name}" for name in STATE_FILES)}
    if (
        not required.issubset(records)
        or not any(name.startswith("workspaces/b/.zvec-grep/") for name in records)
        or (
            config.zvec_grep.embedding.startswith("local/")
            and not any(name.startswith("models/") for name in records)
        )
    ):
        raise ValueError("runtime snapshot is incomplete")
    baseline = manifest.get("baseline")
    if not isinstance(baseline, list):
        raise TypeError("runtime baseline metadata is missing")
    expected = {entry["path"] for entry in corpus_entries(config, artifacts)}
    observed = set()
    for entry in baseline:
        if (
            not isinstance(entry, dict)
            or set(entry) != {"path", "mtime_ns", "mode"}
            or entry["path"] not in expected
            or entry["path"] in observed
            or not isinstance(entry["mtime_ns"], int)
            or not isinstance(entry["mode"], int)
            or not 0 <= entry["mode"] <= 0o777
        ):
            raise ValueError("invalid runtime baseline metadata")
        observed.add(entry["path"])
    if (
        observed != expected
        or {
            name.removeprefix("workspaces/b/")
            for name in records
            if name.startswith("workspaces/b/")
            and not name.startswith("workspaces/b/.zvec-grep/")
        }
        != expected
    ):
        raise ValueError("runtime snapshot corpus files are incomplete")
    for entry in corpus_entries(config, artifacts):
        record = records[f"workspaces/b/{entry['path']}"]
        if record["bytes"] != entry["bytes"] or record["sha256"] != entry["sha256"]:
            raise ValueError("runtime snapshot corpus digest mismatch")
    return records


def restore_baseline(manifest: dict[str, Any], artifacts: Path) -> None:
    root = artifacts / "workspaces/a"
    root.mkdir(parents=True)
    for entry in manifest["baseline"]:
        source = artifacts / "workspaces/b" / entry["path"]
        target = root / entry["path"]
        # Independent files: a baseline write must never change treatment bytes.
        shutil.copy2(source, target)
        target.chmod(entry["mode"])
        os.utime(target, ns=(entry["mtime_ns"], entry["mtime_ns"]))


def prepared_runtime_dataset(config: BenchmarkConfig, artifacts: Path) -> Path:
    """Validate runtime provenance instead of requiring excluded Parquet files."""
    from .dataset import load_queries

    try:
        manifest = read_json(artifacts / MANIFEST_PATH)
        records = validate_manifest(manifest, config, artifacts, manifest["key"])
        for name in (QUERY_PATH, *(f"state/{name}" for name in STATE_FILES)):
            path = artifacts / name
            record = records[name]
            if (
                path.is_symlink()
                or not path.is_file()
                or path.stat().st_size != record["bytes"]
                or sha256_file(path) != record["sha256"]
            ):
                raise ValueError("runtime snapshot state or query digest mismatch")
        state = read_json(artifacts / "state/dataset.json")
        queries = state["queries"]
        corpus = state["corpus"]
        if (
            (
                queries["repo"],
                queries["revision"],
                queries["split"],
                queries["count"],
                corpus["repo"],
                corpus["revision"],
                corpus["split"],
            )
            != (
                config.dataset.queries_repo,
                config.dataset.queries_revision,
                config.dataset.queries_split,
                config.dataset.expected_queries,
                config.dataset.corpus_repo,
                config.dataset.corpus_revision,
                config.dataset.corpus_split,
            )
            or queries["sha256"] != records[QUERY_PATH]["sha256"]
            or len(load_queries(artifacts / QUERY_PATH))
            != config.dataset.expected_queries
        ):
            raise ValueError("runtime dataset identity or count mismatch")
    except (KeyError, OSError, TypeError, ValueError) as error:
        raise RuntimeError(
            "runtime snapshot is invalid; restore a verified snapshot"
        ) from error
    return artifacts / "state/dataset.json"
