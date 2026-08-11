"""Preflight validation for the pinned SWE-QA-Bench task subset."""

from __future__ import annotations

import hashlib
import json
import re
import tomllib
from pathlib import Path
from typing import Any

from . import SweQaError

FULL_COMMIT_RE = re.compile(r"^[0-9a-fA-F]{40}$")


def _object(path: Path, *, label: str) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise SweQaError(f"could not read {label} {path}: {error}") from error
    if not isinstance(value, dict):
        raise SweQaError(f"{label} must be a JSON object: {path}")
    return value


def _records(root: dict[str, Any], key: str, *, label: str) -> list[dict[str, Any]]:
    value = root.get(key)
    if not isinstance(value, list) or any(not isinstance(item, dict) for item in value):
        raise SweQaError(f"{label}.{key} must be an array of objects")
    return value


def _text(record: dict[str, Any], key: str, *, label: str) -> str:
    value = record.get(key)
    if not isinstance(value, str) or not value:
        raise SweQaError(f"{label}.{key} must be a non-empty string")
    return value


def _load_metadata(path: Path) -> dict[str, Any]:
    try:
        root = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        raise SweQaError(f"could not read Harbor task metadata {path}: {error}") from error
    metadata = root.get("metadata")
    if not isinstance(metadata, dict):
        raise SweQaError(f"Harbor task has no metadata table: {path}")
    return metadata


def _instruction_matches(raw: str, question: str) -> bool:
    # A single conventional EOF newline is storage framing, not instruction text.
    return raw == question or raw == question + "\n"


def validate_assets(
    *, selection_path: Path, references_path: Path, dataset_path: Path
) -> dict[str, Any]:
    """Validate the five-task lock, judge references, and Harbor dataset."""
    selection = _object(selection_path, label="selection")
    references = _object(references_path, label="references")
    tasks = _records(selection, "tasks", label="selection")
    reference_records = _records(references, "references", label="references")

    for key in ("benchmark", "benchmark_revision", "index_base"):
        if selection.get(key) != references.get(key):
            raise SweQaError(f"selection and references disagree on {key}")
    if selection.get("index_base") != 0:
        raise SweQaError("selection index_base must be 0")
    if references.get("visibility") != "judge-only":
        raise SweQaError("references visibility must be judge-only")
    if len(tasks) != 5:
        raise SweQaError(f"selection must contain exactly 5 tasks, found {len(tasks)}")
    gate = selection.get("gate")
    if not isinstance(gate, dict) or gate.get("required_tasks") != 5:
        raise SweQaError("selection gate.required_tasks must equal 5")

    ids: set[str] = set()
    slugs: set[str] = set()
    selected: dict[str, dict[str, Any]] = {}
    for index, task in enumerate(tasks):
        label = f"selection.tasks[{index}]"
        task_id = _text(task, "task_id", label=label)
        slug = _text(task, "task_slug", label=label)
        question = _text(task, "question", label=label)
        question_hash = _text(task, "question_hash", label=label)
        commit = _text(task, "repository_commit", label=label)
        if task_id in ids:
            raise SweQaError(f"selection contains duplicate task_id {task_id!r}")
        if slug in slugs:
            raise SweQaError(f"selection contains duplicate task_slug {slug!r}")
        ids.add(task_id)
        slugs.add(slug)
        selected[task_id] = task
        actual_hash = hashlib.sha256(question.encode("utf-8")).hexdigest()
        if question_hash != actual_hash:
            raise SweQaError(f"question SHA256 mismatch for {task_id}")
        if not FULL_COMMIT_RE.fullmatch(commit):
            raise SweQaError(f"repository commit is not a full SHA for {task_id}")

    smoke = [task for task in tasks if task.get("role") == "smoke"]
    categories = [task for task in tasks if task.get("role") == "category"]
    if len(smoke) != 1 or len(categories) != 4:
        raise SweQaError("selection must contain 1 smoke and 4 category tasks")
    if {task.get("category") for task in categories} != {
        "what",
        "where",
        "how",
        "why",
    }:
        raise SweQaError("category tasks must cover what, where, how, and why")

    if not dataset_path.is_dir():
        raise SweQaError(f"Harbor dataset does not exist: {dataset_path}")
    dataset_dirs = {
        path.name
        for path in dataset_path.iterdir()
        if path.is_dir() and not path.name.startswith(".")
    }
    if dataset_dirs != slugs:
        missing = sorted(slugs - dataset_dirs)
        extra = sorted(dataset_dirs - slugs)
        raise SweQaError(
            f"Harbor dataset task mismatch (missing={missing}, extra={extra})"
        )

    metadata_keys = (
        "task_id",
        "question_hash",
        "repository",
        "repository_commit",
        "role",
        "category",
        "question_type",
    )
    for task_id, task in selected.items():
        task_dir = dataset_path / str(task["task_slug"])
        instruction_path = task_dir / "instruction.md"
        try:
            instruction = instruction_path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as error:
            raise SweQaError(
                f"could not read Harbor instruction {instruction_path}: {error}"
            ) from error
        if not _instruction_matches(instruction, str(task["question"])):
            raise SweQaError(f"Harbor instruction does not match selection for {task_id}")

        metadata = _load_metadata(task_dir / "task.toml")
        for key in metadata_keys:
            if metadata.get(key) != task.get(key):
                raise SweQaError(f"Harbor metadata {key} mismatch for {task_id}")
        dockerfile = task_dir / "environment" / "Dockerfile"
        try:
            docker_text = dockerfile.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as error:
            raise SweQaError(f"could not read {dockerfile}: {error}") from error
        if str(task["repository_commit"]) not in docker_text:
            raise SweQaError(f"Dockerfile does not pin the selected commit for {task_id}")

    refs_by_id: dict[str, dict[str, Any]] = {}
    for index, reference in enumerate(reference_records):
        label = f"references.references[{index}]"
        task_id = _text(reference, "task_id", label=label)
        if task_id in refs_by_id:
            raise SweQaError(f"references contains duplicate task_id {task_id!r}")
        refs_by_id[task_id] = reference
    if set(refs_by_id) != ids:
        raise SweQaError("references task IDs do not exactly match selection")
    for task_id, task in selected.items():
        reference = refs_by_id[task_id]
        for key in (
            "question",
            "question_hash",
            "repository",
            "repository_commit",
            "role",
            "category",
        ):
            if reference.get(key) != task.get(key):
                raise SweQaError(f"reference {key} mismatch for {task_id}")
        _text(reference, "reference_answer", label=f"references[{task_id}]")

    dataset_files = sorted(path for path in dataset_path.rglob("*") if path.is_file())
    file_contents: list[tuple[Path, bytes]] = []
    for path in dataset_files:
        try:
            file_contents.append((path, path.read_bytes()))
        except OSError as error:
            raise SweQaError(f"could not scan Harbor dataset file {path}: {error}") from error
    for task_id, reference in refs_by_id.items():
        needle = str(reference["reference_answer"]).encode("utf-8")
        for path, contents in file_contents:
            if needle in contents:
                raise SweQaError(
                    f"judge-only reference answer for {task_id} leaked into "
                    f"Harbor dataset file {path}"
                )

    return {
        "schema_version": 1,
        "valid": True,
        "task_count": len(tasks),
        "task_ids": [str(task["task_id"]) for task in tasks],
        "dataset_files_scanned": len(dataset_files),
        "references_are_judge_only": True,
    }
