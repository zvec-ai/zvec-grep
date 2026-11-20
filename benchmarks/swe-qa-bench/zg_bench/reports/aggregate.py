"""Combine validated task reports without agent or model dependencies."""

from __future__ import annotations

from pathlib import Path
from typing import Any, Sequence

from zg_bench.core.errors import SweQaError
from zg_bench.core.io import load_object
from zg_bench.core.protocol import (
    JUDGE_GENERATION_METADATA_KEYS,
    PROFILE_NAMES,
)
from zg_bench.metrics.numbers import (
    sum_or_none,
)
from zg_bench.metrics.summary import (
    aggregate_cases,
)
from zg_bench.metrics.usage import (
    compatible_usage_scope,
)
from zg_bench.reports.render import (
    write_report,
)
from zg_bench.reports.validation import (
    validate_task_report,
)


def _report_paths(reports_root: Path, output_dir: Path) -> list[Path]:
    output_report = (output_dir / "report.json").resolve()
    if reports_root.is_file():
        paths = [reports_root]
    elif reports_root.is_dir():
        paths = sorted(
            path
            for path in reports_root.rglob("report.json")
            if path.is_file() and path.resolve() != output_report
        )
    else:
        raise SweQaError(f"reports root does not exist: {reports_root}")
    if not paths:
        raise SweQaError(
            f"no per-task report.json files found under reports root: {reports_root}"
        )
    return paths


def _case_judgement_count(case: dict[str, Any]) -> int:
    return sum(
        len(case["profiles"][name]["trials"])
        if "trials" in case["profiles"][name]
        else 1
        for name in PROFILE_NAMES
    )


def _combined_judge(reports: Sequence[dict[str, Any]]) -> dict[str, Any]:
    first = reports[0]["judge"]
    metadata_keys = ("label", "model", "self_judge", "temperature", "rubric")
    metadata = {key: first[key] for key in metadata_keys}
    # Legacy reports without a seed remain readable, but must not be mixed
    # with seeded reports or reports produced with a different seed.
    optional_keys = ("seed", *JUDGE_GENERATION_METADATA_KEYS)
    for key in optional_keys:
        if key in first:
            metadata[key] = first[key]
    for report in reports[1:]:
        judge = report["judge"]
        if any(judge.get(key) != metadata[key] for key in metadata_keys) or any(
            (key in judge) != (key in first) or judge.get(key) != first.get(key)
            for key in optional_keys
        ):
            raise SweQaError("per-task reports use incompatible judge metadata")

    usages = [report["judge"]["usage"] for report in reports]
    metadata["usage"] = {
        "calls": sum(usage["calls"] for usage in usages),
        "input_tokens": sum(usage["input_tokens"] for usage in usages),
        "output_tokens": sum(usage["output_tokens"] for usage in usages),
        "cost_usd": sum_or_none([usage["cost_usd"] for usage in usages]),
    }
    return metadata


def aggregate_reports(
    *,
    reports_root: Path,
    output_dir: Path,
    expected: Sequence[str] | None = None,
    allow_missing: bool = False,
) -> dict[str, Any]:
    """Combine successful one-task reports without making any model calls."""
    source_reports: list[dict[str, Any]] = []
    cases: list[dict[str, Any]] = []
    task_sources: dict[str, Path] = {}
    for path in _report_paths(reports_root, output_dir):
        source = load_object(path, label="per-task report")
        case = validate_task_report(source, path)
        task_id = case["task_id"]
        if task_id in task_sources:
            raise SweQaError(
                f"duplicate task report for {task_id}: "
                f"{task_sources[task_id]} and {path}"
            )
        task_sources[task_id] = path
        source_reports.append(source)
        cases.append(case)

    expected_tasks: list[str] | None = None
    missing: list[str] = []
    if expected is not None:
        expected_tasks = list(expected)
        if (
            not expected_tasks
            or any(
                not isinstance(task, str) or not task.strip() for task in expected_tasks
            )
            or len(set(expected_tasks)) != len(expected_tasks)
        ):
            raise SweQaError("expected aggregate tasks must be non-empty and unique")
        missing = [task for task in expected_tasks if task not in task_sources]
        unexpected = sorted(set(task_sources) - set(expected_tasks))
        if unexpected or (missing and not allow_missing):
            raise SweQaError(
                "aggregate report task mismatch "
                f"(missing={missing}, unexpected={unexpected})"
            )
        cases_by_task = {case["task_id"]: case for case in cases}
        reports_by_task = {
            report["cases"][0]["task_id"]: report for report in source_reports
        }
        completed_tasks = [task for task in expected_tasks if task in cases_by_task]
        cases = [cases_by_task[task] for task in completed_tasks]
        source_reports = [reports_by_task[task] for task in completed_tasks]
    else:
        cases.sort(key=lambda case: case["task_id"])
    task_ids = [case["task_id"] for case in cases]
    successful_judgements = sum(_case_judgement_count(case) for case in cases)
    report = {
        "schema_version": 4,
        "benchmark": "peng-weihan/SWE-QA-Bench",
        "usage_scope": compatible_usage_scope(
            [
                case["profiles"][profile]["metrics"]
                for case in cases
                for profile in PROFILE_NAMES
            ]
        ),
        "judge": _combined_judge(source_reports),
        "gate": {
            "kind": "completion-only",
            "report_only": True,
            "numeric_thresholds": False,
            "expected_tasks": expected_tasks or task_ids,
            "completed_tasks": task_ids,
            "missing_tasks": missing,
            "valid_pairs": len(cases),
            "successful_judgements": successful_judgements,
            "passed": not missing,
        },
        "cases": cases,
        "aggregate": aggregate_cases(cases),
    }
    write_report(report, output_dir)
    return report
