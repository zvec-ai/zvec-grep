"""Preflight validation for the pinned SWE-QA-Bench task subset."""

from __future__ import annotations

import hashlib
import json
import re
import tomllib
from collections import Counter
from pathlib import Path
from typing import Any

from . import SweQaError

FULL_COMMIT_RE = re.compile(r"^[0-9a-fA-F]{40}$")
EXPECTED_TASK_COUNT = 20
EXPECTED_CATEGORIES = ("what", "where", "how", "why")
EXPECTED_TASKS_PER_CATEGORY = 5
EXPECTED_AUTO_TASK_COUNT = 3
EXPECTED_AUTO_CATEGORIES = ("what", "where", "why")


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
    """Validate the twenty-task lock, judge references, and Harbor dataset."""
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
    if len(tasks) != EXPECTED_TASK_COUNT:
        raise SweQaError(
            "selection must contain exactly "
            f"{EXPECTED_TASK_COUNT} tasks, found {len(tasks)}"
        )
    gate = selection.get("gate")
    if not isinstance(gate, dict) or gate.get("required_tasks") != EXPECTED_TASK_COUNT:
        raise SweQaError(
            f"selection gate.required_tasks must equal {EXPECTED_TASK_COUNT}"
        )

    ids: set[str] = set()
    slugs: set[str] = set()
    selected: dict[str, dict[str, Any]] = {}
    for index, task in enumerate(tasks):
        label = f"selection.tasks[{index}]"
        task_id = _text(task, "task_id", label=label)
        slug = _text(task, "task_slug", label=label)
        source_file = _text(task, "source_file", label=label)
        source_index = task.get("source_index")
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
        if slug != task_id.replace(":", "-"):
            raise SweQaError(f"non-canonical task_slug for {task_id}")
        if (
            isinstance(source_index, bool)
            or not isinstance(source_index, int)
            or source_index < 0
            or task_id != f"{Path(source_file).stem}:{source_index}"
        ):
            raise SweQaError(f"source index does not match task_id for {task_id}")
        actual_hash = hashlib.sha256(question.encode("utf-8")).hexdigest()
        if question_hash != actual_hash:
            raise SweQaError(f"question SHA256 mismatch for {task_id}")
        if not FULL_COMMIT_RE.fullmatch(commit):
            raise SweQaError(f"repository commit is not a full SHA for {task_id}")

    smoke = [task for task in tasks if task.get("role") == "smoke"]
    categories = [task for task in tasks if task.get("role") == "category"]
    if len(smoke) != 1 or len(categories) != EXPECTED_TASK_COUNT - 1:
        raise SweQaError("selection must contain 1 smoke and 19 category tasks")

    category_counts: Counter[str] = Counter()
    for task in tasks:
        category = task.get("category")
        if category not in EXPECTED_CATEGORIES:
            raise SweQaError(f"invalid question category for {task.get('task_id')}")
        if task.get("question_type") != category:
            raise SweQaError(
                f"question_type does not match category for {task.get('task_id')}"
            )
        category_counts[str(category)] += 1
    expected_counts = {
        category: EXPECTED_TASKS_PER_CATEGORY for category in EXPECTED_CATEGORIES
    }
    if dict(category_counts) != expected_counts:
        raise SweQaError("selection must contain 5 tasks in each question category")

    smoke_task_id = str(smoke[0]["task_id"])
    if gate.get("smoke_task") != smoke_task_id:
        raise SweQaError("selection gate.smoke_task does not match the smoke task")
    auto_tasks = gate.get("auto_tasks")
    if not isinstance(auto_tasks, list) or any(
        not isinstance(task_id, str) or not task_id for task_id in auto_tasks
    ):
        raise SweQaError("selection gate.auto_tasks must be an array of task IDs")
    if len(auto_tasks) != EXPECTED_AUTO_TASK_COUNT:
        raise SweQaError(
            "selection gate.auto_tasks must contain exactly "
            f"{EXPECTED_AUTO_TASK_COUNT} tasks"
        )
    if len(set(auto_tasks)) != len(auto_tasks):
        raise SweQaError("selection gate.auto_tasks must not contain duplicates")
    unknown_auto_tasks = sorted(set(auto_tasks) - ids)
    if unknown_auto_tasks:
        raise SweQaError(
            "selection gate.auto_tasks contains unknown task IDs: "
            f"{unknown_auto_tasks}"
        )
    auto_category_tasks = [selected[task_id] for task_id in auto_tasks]
    if any(task.get("role") != "category" for task in auto_category_tasks):
        raise SweQaError("selection gate.auto_tasks entries must be category tasks")
    auto_categories = [str(task.get("category")) for task in auto_category_tasks]
    if tuple(auto_categories) != EXPECTED_AUTO_CATEGORIES:
        raise SweQaError(
            "selection gate.auto_tasks must contain one category task in "
            "what/where/why order"
        )
    expected_category_tasks = [
        str(task["task_id"]) for task in tasks if task.get("role") == "category"
    ]
    if gate.get("category_tasks") != expected_category_tasks:
        raise SweQaError(
            "selection gate.category_tasks must list all non-smoke tasks in order"
        )

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
        expected_metadata = {
            "benchmark": selection["benchmark"],
            "benchmark_revision": selection["benchmark_revision"],
            "index_base": selection["index_base"],
            "task_id": task["task_id"],
            "source_index": task["source_index"],
            "question_hash": task["question_hash"],
            "repository": task["repository"],
            "repository_commit": task["repository_commit"],
            "role": task["role"],
            "category": task["category"],
            "question_type": task["question_type"],
        }
        for key, expected_value in expected_metadata.items():
            if metadata.get(key) != expected_value:
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
            "source_file",
            "source_index",
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
        "auto_task_count": len(auto_tasks),
        "auto_task_ids": auto_tasks,
        "dataset_files_scanned": len(dataset_files),
        "references_are_judge_only": True,
    }
