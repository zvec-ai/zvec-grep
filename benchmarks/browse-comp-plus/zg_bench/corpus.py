from __future__ import annotations

import hashlib
import json
import shutil
from functools import lru_cache
from pathlib import Path
from typing import Any, Callable, Iterator

import pyarrow.parquet as parquet

from .artifacts import fingerprint, read_json, sha256_file, utc_now, write_json
from .config import BenchmarkConfig


WORKSPACE_NAMES = {"baseline": "a", "zvec-grep": "b"}


def workspace_root(artifacts: Path, profile: str) -> Path:
    try:
        name = WORKSPACE_NAMES[profile]
    except KeyError as error:
        raise ValueError(f"unknown benchmark profile: {profile}") from error
    return (artifacts / "workspaces" / name).resolve()


def _rows(files: list[Path]) -> Iterator[dict[str, Any]]:
    for path in files:
        source = parquet.ParquetFile(path)
        for batch in source.iter_batches(
            batch_size=256, columns=["docid", "text", "url"]
        ):
            yield from batch.to_pylist()


def _safe_docid(value: Any) -> str:
    docid = str(value)
    if (
        not docid
        or docid in {".", ".."}
        or "/" in docid
        or "\\" in docid
        or "\0" in docid
    ):
        raise ValueError(f"unsafe corpus docid: {docid!r}")
    return docid


@lru_cache(maxsize=4)
def _manifest_state_cached(
    manifest_path: str,
    mtime_ns: int,
    size: int,
    source_revision: str,
) -> tuple[frozenset[str], str]:
    del mtime_ns, size
    files: set[str] = set()
    digest = hashlib.sha256()
    digest.update(source_revision.encode("utf-8"))
    digest.update(b"\0")
    with Path(manifest_path).open(encoding="utf-8") as manifest:
        for line in manifest:
            entry = json.loads(line)
            docid = _safe_docid(entry["docid"])
            relative = Path(str(entry["path"]))
            if (
                relative.name != relative.as_posix()
                or relative.suffix != ".md"
                or relative.name != f"{docid}.md"
            ):
                raise RuntimeError(
                    f"invalid corpus path in manifest: {relative}"
                )
            files.add(relative.name)
            for value in (docid, entry["sha256"]):
                digest.update(str(value).encode("utf-8"))
                digest.update(b"\0")
    return frozenset(files), digest.hexdigest()


def _manifest_files(manifest: Path, state: dict[str, Any]) -> frozenset[str]:
    metadata = manifest.stat()
    files, actual_fingerprint = _manifest_state_cached(
        str(manifest.resolve()),
        metadata.st_mtime_ns,
        metadata.st_size,
        str(state["source_revision"]),
    )
    if (
        len(files) != int(state["count"])
        or actual_fingerprint != state["fingerprint"]
    ):
        raise RuntimeError("corpus manifest does not match its prepared state")
    return files


def _workspace_state(
    root: Path,
    corpus_files: frozenset[str] | set[str],
    *,
    retained: frozenset[str] = frozenset(),
) -> tuple[dict[str, int | str], list[Path]]:
    digest = hashlib.sha256()
    entries = 0
    total_bytes = 0
    residuals: list[Path] = []
    for path in sorted(root.iterdir(), key=lambda item: item.name):
        if path.name not in corpus_files:
            if path.name not in retained:
                residuals.append(path)
            continue
        metadata = path.lstat()
        kind = (
            "symlink"
            if path.is_symlink()
            else "file"
            if path.is_file()
            else "other"
        )
        digest.update(
            f"{path.name}\0{kind}\0{metadata.st_size}\0{metadata.st_mtime_ns}\0".encode()
        )
        entries += 1
        total_bytes += metadata.st_size
    return (
        {
            "entries": entries,
            "total_bytes": total_bytes,
            "metadata_fingerprint": digest.hexdigest(),
        },
        residuals,
    )


def _remove_residuals(paths: list[Path]) -> None:
    for path in paths:
        try:
            if path.is_symlink() or not path.is_dir():
                path.unlink(missing_ok=True)
            else:
                shutil.rmtree(path)
        except OSError as error:
            raise RuntimeError(
                f"could not remove workspace residue {path}: {error}"
            ) from error


def prepared_corpus(config: BenchmarkConfig, artifacts: Path) -> Path | None:
    state_path = artifacts / "state" / "corpus.json"
    if not state_path.is_file():
        return None
    try:
        state = read_json(state_path)
        identity = (state["source_revision"], state["count"])
    except (KeyError, TypeError, ValueError):
        return None
    expected = (
        config.dataset.corpus_revision,
        config.dataset.expected_corpus_documents,
    )
    if identity != expected:
        raise RuntimeError(
            "materialized corpus does not match benchmark.toml; "
            "start with an empty artifacts directory"
        )
    baseline = workspace_root(artifacts, "baseline")
    treatment = workspace_root(artifacts, "zvec-grep")
    manifest = artifacts / "state" / "corpus-manifest.jsonl"
    workspaces = state.get("workspaces")
    if (
        not isinstance(workspaces, dict)
        or workspaces.get("baseline") != str(baseline)
        or workspaces.get("zvec-grep") != str(treatment)
        or state.get("manifest") != str(manifest.resolve())
        or not baseline.is_dir()
        or not treatment.is_dir()
        or not manifest.is_file()
    ):
        return None
    try:
        corpus_files = _manifest_files(manifest, state)
    except (KeyError, OSError, TypeError, ValueError, RuntimeError):
        return None
    baseline_snapshot, baseline_residuals = _workspace_state(
        baseline, corpus_files
    )
    treatment_snapshot, treatment_residuals = _workspace_state(
        treatment,
        corpus_files,
        retained=frozenset({".zvec-grep"}),
    )
    _remove_residuals(baseline_residuals)
    _remove_residuals(treatment_residuals)
    snapshots = state.get("workspace_snapshots")
    if (
        not isinstance(snapshots, dict)
        or any(
            not isinstance(snapshot, dict)
            or snapshot.get("entries")
            != config.dataset.expected_corpus_documents
            for snapshot in snapshots.values()
        )
        or snapshots
        != {
            "baseline": baseline_snapshot,
            "zvec-grep": treatment_snapshot,
        }
    ):
        return None
    return state_path


def validate_workspace(artifacts: Path, profile: str) -> None:
    state_path = artifacts / "state" / "corpus.json"
    if not state_path.is_file():
        raise RuntimeError("corpus state is missing")
    state = read_json(state_path)
    snapshots = state.get("workspace_snapshots")
    if not isinstance(snapshots, dict) or not isinstance(
        snapshots.get(profile), dict
    ):
        raise RuntimeError(f"corpus state is missing the {profile} snapshot")
    root = workspace_root(artifacts, profile)
    if not root.is_dir():
        raise RuntimeError(f"{profile} workspace is missing")
    manifest = artifacts / "state" / "corpus-manifest.jsonl"
    try:
        corpus_files = _manifest_files(manifest, state)
    except (KeyError, OSError, TypeError, ValueError) as error:
        raise RuntimeError("corpus manifest is invalid") from error
    retained = (
        frozenset({".zvec-grep"})
        if profile == "zvec-grep"
        else frozenset()
    )
    snapshot, residuals = _workspace_state(
        root, corpus_files, retained=retained
    )
    _remove_residuals(residuals)
    if snapshot != snapshots[profile]:
        raise RuntimeError(
            f"{profile} Markdown corpus changed during benchmark execution; "
            "restore the prepared corpus before continuing"
        )


def materialize(
    config: BenchmarkConfig,
    artifacts: Path,
    *,
    progress: Callable[[str, int, int], None] | None = None,
) -> Path:
    source = artifacts / "source" / "corpus"
    files = sorted(source.rglob("*.parquet"))
    if not files:
        raise RuntimeError("corpus source is missing; run 'zg-bench prepare'")

    baseline = workspace_root(artifacts, "baseline")
    treatment = workspace_root(artifacts, "zvec-grep")
    baseline.mkdir(parents=True, exist_ok=True)
    treatment.mkdir(parents=True, exist_ok=True)
    manifest_path = artifacts / "state" / "corpus-manifest.jsonl"
    temporary_manifest = manifest_path.with_suffix(".jsonl.tmp")
    temporary_manifest.parent.mkdir(parents=True, exist_ok=True)

    count = 0
    total_bytes = 0
    maximum_bytes = 0
    corpus_files: set[str] = set()
    aggregate_parts: list[str] = [config.dataset.corpus_revision]
    expected_count = config.dataset.expected_corpus_documents
    if progress:
        progress("Baseline", 0, expected_count)
    with temporary_manifest.open("w", encoding="utf-8") as manifest:
        for row in _rows(files):
            docid = _safe_docid(row["docid"])
            text = str(row["text"])
            encoded = text.encode("utf-8")
            digest = hashlib.sha256(encoded).hexdigest()
            relative = Path(f"{docid}.md")
            output = baseline / relative
            if not output.is_file() or sha256_file(output) != digest:
                output.parent.mkdir(parents=True, exist_ok=True)
                output.write_text(text, encoding="utf-8")
            entry = {
                "docid": docid,
                "url": str(row["url"]),
                "path": relative.as_posix(),
                "bytes": len(encoded),
                "sha256": digest,
            }
            manifest.write(json.dumps(entry, ensure_ascii=False) + "\n")
            corpus_files.add(relative.name)
            count += 1
            total_bytes += len(encoded)
            maximum_bytes = max(maximum_bytes, len(encoded))
            aggregate_parts.extend((docid, digest))
            if progress and count % 1000 == 0:
                progress("Baseline", count, expected_count)

    if progress and count % 1000:
        progress("Baseline", count, expected_count)
    if count != expected_count:
        temporary_manifest.unlink(missing_ok=True)
        raise RuntimeError(
            f"expected {expected_count} corpus documents, found {count}"
        )
    if sum(1 for path in baseline.glob("*.md") if path.is_file()) != count:
        raise RuntimeError(
            "baseline workspace contains unexpected Markdown files; "
            "start with an empty artifacts directory"
        )

    if progress:
        progress("Treatment", 0, expected_count)
    copied = 0
    with temporary_manifest.open(encoding="utf-8") as manifest:
        for line in manifest:
            entry = json.loads(line)
            source_path = baseline / entry["path"]
            target_path = treatment / entry["path"]
            target_digest = sha256_file(target_path) if target_path.is_file() else None
            if target_digest != entry["sha256"]:
                shutil.copy2(source_path, target_path)
                target_digest = sha256_file(target_path)
            if target_digest != entry["sha256"]:
                raise RuntimeError(
                    f"copied corpus file failed verification: {target_path}"
                )
            copied += 1
            if progress and copied % 1000 == 0:
                progress("Treatment", copied, expected_count)
    if progress and copied % 1000:
        progress("Treatment", copied, expected_count)
    if sum(1 for path in treatment.glob("*.md") if path.is_file()) != count:
        raise RuntimeError(
            "treatment workspace contains unexpected Markdown files; "
            "start with an empty artifacts directory"
        )

    unexpected = {
        profile: [
            path.name
            for path in root.iterdir()
            if path.name != ".zvec-grep"
            and (not path.is_file() or path.suffix != ".md")
        ][:5]
        for profile, root in (("baseline", baseline), ("zvec-grep", treatment))
    }
    for profile, names in unexpected.items():
        if names:
            raise RuntimeError(
                f"{profile} workspace contains unexpected entries: "
                + ", ".join(names)
                + "; start with an empty artifacts directory"
            )

    temporary_manifest.replace(manifest_path)
    state = {
        "stage": "materialize",
        "generated_at": utc_now(),
        "source_revision": config.dataset.corpus_revision,
        "workspaces": {
            "baseline": str(baseline),
            "zvec-grep": str(treatment),
        },
        "manifest": str(manifest_path.resolve()),
        "count": count,
        "total_bytes": total_bytes,
        "maximum_file_bytes": maximum_bytes,
        "fingerprint": fingerprint(aggregate_parts),
        "workspace_snapshots": {
            "baseline": _workspace_state(baseline, corpus_files)[0],
            "zvec-grep": _workspace_state(
                treatment,
                corpus_files,
                retained=frozenset({".zvec-grep"}),
            )[0],
        },
        "content_policy": "Parquet text fields are written byte-for-byte as UTF-8",
    }
    output = artifacts / "state" / "corpus.json"
    write_json(output, state)
    return output
