"""One-pass task screening from residual dispersion after score trimming.

This labels relative dispersion candidates, not invalid trials or significant
version differences. Callers supply the comparison groups; no history is loaded.
"""

from __future__ import annotations

import math
import statistics
from collections.abc import Sequence
from typing import Any

from zg_bench.core.errors import SweQaError
from zg_bench.metrics.numbers import sum_or_none

DISPERSION_METRICS = ("judge", "agent_wall_seconds", "input_tokens")
TASK_SCREEN_POLICY = {
    "method": "task_max_residual_dispersion_mad",
    "score_dispersion": "sample_standard_deviation",
    "resource_dispersion": "sample_standard_deviation_over_arithmetic_mean",
    "ddof": 1,
    "task_statistic": "maximum_over_supplied_profiles",
    "reference": "median_and_unscaled_mad_over_all_task_maxima",
    "coefficient": 0.6745,
    "cutoff": 3.5,
    "direction": "upper_tail_strictly_greater",
    "minimum_tasks": 5,
    "minimum_tasks_reason": "engineering_guard_not_a_statistical_standard",
    "missing_metric": "disable_metric_for_all_tasks",
    "zero_mad": "disable_metric_for_all_tasks",
    "passes": 1,
    "refit_after_exclusion": False,
    "paired_task_exclusion": "all_supplied_profiles",
    "interpretation": "relative_extreme_dispersion_candidates_not_invalid_data",
    "error_control": "no_calibrated_familywise_error_guarantee",
}


def _finite_nonnegative(value: Any) -> bool:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return False
    try:
        return math.isfinite(value) and value >= 0
    except OverflowError:
        return False


def residual_dispersion(
    retained_trials: Sequence[dict[str, Any]] | None,
) -> dict[str, Any]:
    """Measure the same retained tuples used for every reported trial mean."""
    count = len(retained_trials) if retained_trials is not None else None
    results = {}
    for metric in DISPERSION_METRICS:
        row = {
            "status": "available",
            "value": None,
            "sample_count": count,
            "mean": None,
            "sample_sd": None,
        }
        results[metric] = row
        if retained_trials is None:
            row["status"] = "unavailable_no_trial_evidence"
            continue
        if len(retained_trials) < 2:
            row["status"] = "unavailable_insufficient_samples"
            continue
        values = [
            trial.get("judge", {}).get("total")
            if metric == "judge"
            else trial.get("metrics", {}).get(metric)
            for trial in retained_trials
        ]
        if any(value is None for value in values):
            row["status"] = "unavailable_missing_values"
            continue
        if not all(_finite_nonnegative(value) for value in values):
            row["status"] = "unavailable_invalid_values"
            continue
        try:
            mean = statistics.mean(values)
            sd = statistics.stdev(values)
            if not _finite_nonnegative(mean) or not _finite_nonnegative(sd):
                raise ArithmeticError("non-finite dispersion")
            row.update(mean=mean, sample_sd=sd)
            if metric != "judge" and mean == 0:
                row["status"] = "unavailable_zero_mean"
                continue
            value = sd if metric == "judge" else sd / mean
            if not _finite_nonnegative(value):
                raise ArithmeticError("non-finite dispersion")
            row["value"] = value
        except (ArithmeticError, OverflowError, ValueError):
            row.update(status="unavailable_invalid_values", value=None)
    return results


def _task_maximum(profiles: dict[str, Any], metric: str) -> dict[str, Any]:
    unavailable = []
    values = {}
    for name in sorted(profiles):
        dispersion = profiles[name].get("dispersion", {}).get(metric, {})
        status = dispersion.get("status", "unavailable_missing_dispersion")
        value = dispersion.get("value")
        if status == "available" and not _finite_nonnegative(value):
            status = "unavailable_invalid_values"
        if status != "available":
            unavailable.append({"profile": name, "status": status})
        else:
            values[name] = value
    if unavailable:
        return {
            "status": "unavailable_missing_dispersion",
            "value": None,
            "source_profile": None,
            "modified_z": None,
            "unavailable_profiles": unavailable,
        }
    name = max(values, key=values.__getitem__)
    return {
        "status": "available",
        "value": values[name],
        "source_profile": name,
        "modified_z": None,
        "unavailable_profiles": [],
    }


def _metric_reference(decisions: dict[str, Any], metric: str) -> dict[str, Any]:
    missing = [
        task_id
        for task_id, row in decisions.items()
        if row["metrics"][metric]["status"] != "available"
    ]
    reference = {
        "status": "available",
        "median": None,
        "mad": None,
        "upper_threshold": None,
        "unavailable_task_ids": missing,
    }
    if len(decisions) < TASK_SCREEN_POLICY["minimum_tasks"]:
        reference["status"] = "unavailable_insufficient_tasks"
        return reference
    if missing:
        reference["status"] = "unavailable_missing_dispersion"
        return reference
    values = [row["metrics"][metric]["value"] for row in decisions.values()]
    center = statistics.median(values)
    mad = statistics.median(abs(value - center) for value in values)
    if not _finite_nonnegative(center) or not _finite_nonnegative(mad):
        reference["status"] = "unavailable_invalid_reference"
        return reference
    reference.update(median=center, mad=mad)
    if mad == 0:
        reference["status"] = "unavailable_zero_mad"
        return reference
    coefficient = TASK_SCREEN_POLICY["coefficient"]
    cutoff = TASK_SCREEN_POLICY["cutoff"]
    threshold = center + cutoff * mad / coefficient
    standardized = [coefficient * (value - center) / mad for value in values]
    if not math.isfinite(threshold) or not all(map(math.isfinite, standardized)):
        reference["status"] = "unavailable_invalid_reference"
        return reference
    reference["upper_threshold"] = threshold
    for row, z in zip(decisions.values(), standardized, strict=True):
        row["metrics"][metric]["modified_z"] = z
    return reference


def screen_tasks(tasks: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """Screen supplied task statistics once without changing source evidence.

    Every task must provide the same named profiles, so its maximum is taken
    over an equal set of comparison groups. One unavailable group disables that
    metric across the suite; other complete metrics can still be used.
    """
    decisions = {}
    profile_names: set[str] | None = None
    for task in tasks:
        task_id = task.get("task_id")
        if not isinstance(task_id, str) or not task_id or task_id in decisions:
            raise SweQaError("task screen requires non-empty, unique task IDs")
        profiles = task.get("profiles")
        if not isinstance(profiles, dict) or not profiles:
            raise SweQaError("task screen requires non-empty profile groups")
        names = set(profiles)
        if not all(isinstance(name, str) and name for name in names):
            raise SweQaError("task screen requires non-empty profile names")
        if profile_names is not None and names != profile_names:
            raise SweQaError(
                "task screen requires the same profile groups for all tasks"
            )
        profile_names = names
        decisions[task_id] = {
            "status": "unscreened",
            "reasons": [],
            "metrics": {
                metric: _task_maximum(profiles, metric) for metric in DISPERSION_METRICS
            },
        }
    references = {
        metric: _metric_reference(decisions, metric) for metric in DISPERSION_METRICS
    }
    available = [
        metric for metric, row in references.items() if row["status"] == "available"
    ]
    cutoff = TASK_SCREEN_POLICY["cutoff"]
    for row in decisions.values():
        for metric in available:
            measure = row["metrics"][metric]
            if measure["modified_z"] > cutoff:
                row["reasons"].append(
                    {
                        "metric": metric,
                        "source_profile": measure["source_profile"],
                        "dispersion": measure["value"],
                        "modified_z": measure["modified_z"],
                        "upper_threshold": references[metric]["upper_threshold"],
                    }
                )
        row["status"] = (
            "excluded" if row["reasons"] else "retained" if available else "unscreened"
        )
    excluded = [
        task_id for task_id, row in decisions.items() if row["status"] == "excluded"
    ]
    retained = [task_id for task_id in decisions if task_id not in excluded]

    def total(name: str, field: str, selected: Sequence[dict[str, Any]]) -> int | None:
        return sum_or_none(
            [task["profiles"][name].get("filter", {}).get(field) for task in selected]
        )

    retained_tasks = [task for task in tasks if task["task_id"] in retained]
    counts = {
        name: {
            "original_count": total(name, "original_count", tasks),
            "after_trim_count": total(name, "retained_count", tasks),
            "final_count": total(name, "retained_count", retained_tasks),
        }
        for name in sorted(profile_names or ())
    }
    if len(tasks) < TASK_SCREEN_POLICY["minimum_tasks"]:
        status = "unavailable_insufficient_tasks"
    elif not available:
        status = "unavailable_no_metrics"
    elif len(available) == len(DISPERSION_METRICS):
        status = "applied"
    else:
        status = "partially_applied"
    return {
        "status": status,
        "policy": dict(TASK_SCREEN_POLICY),
        "original_task_count": len(tasks),
        "retained_task_count": len(retained),
        "excluded_task_count": len(excluded),
        "original_task_ids": list(decisions),
        "retained_task_ids": retained,
        "excluded_task_ids": excluded,
        "references": references,
        "task_decisions": decisions,
        "profiles": counts,
    }
