"""Validate the five self-authored symbol probes without changing the QA20 lock."""
from __future__ import annotations

import hashlib
from pathlib import Path

from . import SweQaError
from .validation import FULL_COMMIT_RE, _instruction_matches, _load_metadata, _object, _records, _text

TASK_IDS = ("requests:cc1", "matplotlib:cc1", "sympy:cc1", "matplotlib:cc2", "sympy:cc2")
SMOKE_TASK_IDS = ("requests:cc1", "matplotlib:cc2")


def _read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as error:
        raise SweQaError(f"could not read {path}: {error}") from error


def validate_symbol_assets(*, selection_path: Path, references_path: Path, dataset_path: Path) -> dict[str, object]:
    selection = _object(selection_path, label="selection")
    references = _object(references_path, label="references")
    tasks = _records(selection, "tasks", label="selection")
    refs = _records(references, "references", label="references")
    for key in ("suite", "benchmark", "benchmark_revision", "index_base"):
        if selection.get(key) != references.get(key):
            raise SweQaError(f"selection and references disagree on {key}")
    if selection.get("suite") != "caller-callee-symbols" or selection.get("index_base") != 0:
        raise SweQaError("expected the caller-callee-symbols suite with index_base=0")
    if references.get("visibility") != "judge-only":
        raise SweQaError("references must be judge-only")
    if tuple(t.get("task_id") for t in tasks) != TASK_IDS:
        raise SweQaError("selection must contain the five fixed symbol tasks in order")
    if tuple(r.get("task_id") for r in refs) != TASK_IDS:
        raise SweQaError("references must match the five fixed symbol tasks in order")
    expected_gate = {"required_tasks": 5, "smoke_task": TASK_IDS[0], "auto_tasks": list(SMOKE_TASK_IDS), "full_tasks": list(TASK_IDS)}
    if selection.get("gate") != expected_gate:
        raise SweQaError("symbol task gate differs from the fixed smoke/full selection")
    slugs = {task_id.replace(":", "-") for task_id in TASK_IDS}
    if not dataset_path.is_dir() or {p.name for p in dataset_path.iterdir() if p.is_dir() and not p.name.startswith(".")} != slugs:
        raise SweQaError("Harbor dataset must contain exactly the five symbol tasks")

    for task, reference in zip(tasks, refs, strict=True):
        task_id = task["task_id"]
        question = _text(task, "question", label=task_id)
        commit = _text(task, "repository_commit", label=task_id)
        repository = _text(task, "repository", label=task_id)
        if not FULL_COMMIT_RE.fullmatch(commit):
            raise SweQaError(f"repository commit is not a full SHA: {task_id}")
        if task.get("question_hash") != hashlib.sha256(question.encode()).hexdigest():
            raise SweQaError(f"question SHA256 mismatch: {task_id}")
        expected = {"task_slug": task_id.replace(":", "-"), "source_file": "self-authored/caller-callee", "source_index": None,
                    "provenance": "self-authored", "scenario": "anchor", "category": "where", "question_type": "where",
                    "role": "smoke" if task_id == TASK_IDS[0] else "task", "repository_url": f"https://github.com/{repository}.git"}
        if any(task.get(key) != value for key, value in expected.items()):
            raise SweQaError(f"invalid self-authored task metadata: {task_id}")
        for key in ("task_id", "question", "question_hash", "repository", "repository_commit", "source_file", "source_index", "provenance", "role", "scenario", "category"):
            if reference.get(key) != task.get(key):
                raise SweQaError(f"reference {key} mismatch: {task_id}")
        _text(reference, "reference_answer", label=task_id)
        task_dir = dataset_path / task["task_slug"]
        if not _instruction_matches(_read_text(task_dir / "instruction.md"), question):
            raise SweQaError(f"Harbor instruction does not match selection: {task_id}")
        metadata = _load_metadata(task_dir / "task.toml")
        if metadata.get("source_index") is not None:
            raise SweQaError(f"self-authored Harbor task must omit source_index: {task_id}")
        for key in ("benchmark", "benchmark_revision", "index_base"):
            if metadata.get(key) != selection[key]:
                raise SweQaError(f"Harbor metadata {key} mismatch: {task_id}")
        for key in ("task_id", "question_hash", "repository", "repository_commit", "role", "scenario", "category", "question_type", "provenance"):
            if metadata.get(key) != task[key]:
                raise SweQaError(f"Harbor metadata {key} mismatch: {task_id}")
        dockerfile = _read_text(task_dir / "environment/Dockerfile")
        if f"git -C /app fetch --depth=1 origin {commit}" not in dockerfile or task["repository_url"] not in dockerfile:
            raise SweQaError(f"Dockerfile does not pin the selected source: {task_id}")

    files = [(path, path.read_bytes()) for path in dataset_path.rglob("*") if path.is_file()]
    for reference in refs:
        for path, contents in files:
            if reference["reference_answer"].encode() in contents:
                raise SweQaError(f"judge-only reference leaked into {path}")
    return {"valid": True, "suite": "caller-callee-symbols", "task_count": 5, "task_ids": list(TASK_IDS), "references_are_judge_only": True}
