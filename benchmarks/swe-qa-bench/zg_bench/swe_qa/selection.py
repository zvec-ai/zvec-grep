"""CI task scopes derived from the locked SWE-QA selection."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from zg_bench.core.errors import SweQaError
from zg_bench.core.io import load_object

# Relative low, medium, and high path variability in the historical
# 35206585943 run. These are fixed IDs, not a fresh stability classification.
REPRO_TASK_IDS = ("reflex:6", "requests:16", "conan:39")
CI_SCOPES = ("repro-3", "smoke", "all-full", "gate-20")


def select_ci_tasks(
    selection: dict[str, Any], scope: str
) -> tuple[list[str], list[str]]:
    tasks = selection["tasks"]
    tasks_by_id = {task["task_id"]: task for task in tasks}
    if scope == "smoke":
        task_ids = list(selection["gate"]["auto_tasks"])
    elif scope == "repro-3":
        task_ids = list(REPRO_TASK_IDS)
    elif scope in {"all-full", "gate-20"}:
        task_ids = [task["task_id"] for task in tasks]
    else:
        raise SweQaError(f"unsupported SWE-QA scope: {scope}")
    if not task_ids:
        raise SweQaError(f"SWE-QA scope {scope} selected no tasks")
    if len(task_ids) != len(set(task_ids)):
        raise SweQaError(f"SWE-QA scope {scope} selected duplicate tasks")
    missing = sorted(set(task_ids) - tasks_by_id.keys())
    if missing:
        raise SweQaError(f"SWE-QA scope {scope} selected unknown tasks: {missing}")
    return task_ids, [tasks_by_id[task_id]["task_slug"] for task_id in task_ids]


def matrix_outputs(selection_path: Path, scope: str) -> str:
    task_ids, slugs = select_ci_tasks(
        load_object(selection_path, label="selection"), scope
    )
    return (
        "\n".join(
            (
                f"tasks={json.dumps(slugs, separators=(',', ':'))}",
                f"count={len(slugs)}",
                f"task_ids_json={json.dumps(task_ids, separators=(',', ':'))}",
            )
        )
        + "\n"
    )
