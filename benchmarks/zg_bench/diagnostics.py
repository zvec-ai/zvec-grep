from __future__ import annotations

import json
from pathlib import Path
from typing import Any


def latest_job(jobs_dir: Path) -> Path | None:
    if not jobs_dir.is_dir():
        return None
    jobs = [path for path in jobs_dir.iterdir() if path.is_dir()]
    if not jobs:
        return None
    return max(jobs, key=lambda path: path.stat().st_mtime)


def format_job_diagnostics(job_dir: Path, *, max_trials: int = 3) -> str:
    job_dir = job_dir.resolve()
    lines = [f"Failure details: {job_dir}"]
    if not job_dir.is_dir():
        lines.append("  Job directory was not created.")
        return "\n".join(lines)

    result = _read_json(job_dir / "result.json")
    exception_stats = _exception_stats(result)
    if exception_stats:
        summary = ", ".join(
            f"{name}={count}" for name, count in sorted(exception_stats.items())
        )
        lines.append(f"  Exceptions: {summary}")

    trial_dirs = sorted(
        {path.parent for path in job_dir.glob("*/exception.txt")},
        key=lambda path: path.name,
    )
    setup_files = sorted(job_dir.glob("*/agent/zvec-grep-setup.json"))
    setup_by_trial = {path.parents[1]: path for path in setup_files}

    if not trial_dirs and not setup_files:
        job_log = job_dir / "job.log"
        if job_log.is_file():
            lines.append("  Recent job log:")
            lines.extend(_indented_tail(job_log.read_text(encoding="utf-8"), 12))
        else:
            lines.append("  No trial exception or setup metadata was found.")
        return "\n".join(lines)

    all_trials = sorted(
        set(trial_dirs) | set(setup_by_trial), key=lambda path: path.name
    )
    for trial_dir in all_trials[:max_trials]:
        lines.append(f"  Trial: {trial_dir.name}")
        setup_path = setup_by_trial.get(trial_dir)
        if setup_path is not None:
            setup = _read_json(setup_path)
            if setup:
                stage = setup.get("status", "unknown")
                error_type = setup.get("error_type")
                error = setup.get("error")
                detail = f"setup={stage}"
                if error_type:
                    detail += f", {error_type}"
                lines.append(f"    {detail}")
                if isinstance(error, str) and error.strip():
                    lines.extend(_indented_tail(error, 8, indent="      "))

        exception_path = trial_dir / "exception.txt"
        if exception_path.is_file():
            lines.append("    Exception tail:")
            lines.extend(
                _indented_tail(
                    exception_path.read_text(encoding="utf-8"),
                    14,
                    indent="      ",
                )
            )

    omitted = len(all_trials) - max_trials
    if omitted > 0:
        lines.append(f"  ... {omitted} additional failed trial(s) omitted")
    return "\n".join(lines)


def _read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    return value if isinstance(value, dict) else {}


def _exception_stats(result: dict[str, Any]) -> dict[str, int]:
    stats = result.get("stats")
    if not isinstance(stats, dict):
        return {}
    evals = stats.get("evals")
    if not isinstance(evals, dict):
        return {}

    counts: dict[str, int] = {}
    for evaluation in evals.values():
        if not isinstance(evaluation, dict):
            continue
        exceptions = evaluation.get("exception_stats")
        if not isinstance(exceptions, dict):
            continue
        for name, trials in exceptions.items():
            if isinstance(name, str) and isinstance(trials, list):
                counts[name] = counts.get(name, 0) + len(trials)
    return counts


def _indented_tail(
    value: str, line_count: int, *, indent: str = "    "
) -> list[str]:
    lines = [line.rstrip() for line in value.strip().splitlines()]
    return [f"{indent}{line}" for line in lines[-line_count:]]
