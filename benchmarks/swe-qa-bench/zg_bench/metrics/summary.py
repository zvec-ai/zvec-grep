"""Raw trial evidence and fixed-tail score statistics."""

from __future__ import annotations

from typing import Any, Sequence

from zg_bench.core.errors import SweQaError
from zg_bench.core.protocol import (
    PROFILE_NAMES,
    SCORE_KEYS,
)
from zg_bench.metrics.comparison import (
    compare_profiles,
)
from zg_bench.metrics.numbers import (
    mean_or_none,
    sum_or_none,
)
from zg_bench.metrics.usage import (
    compatible_usage_scope,
    summarize_usage,
)


def summarize_profile(trials: Sequence[dict[str, Any]]) -> dict[str, Any]:
    count = len(trials)
    identity = {key: trials[0]["judge"][key] for key in ("label", "model")}
    if any(
        trial["judge"].get(key) != value
        for trial in trials
        for key, value in identity.items()
    ):
        raise SweQaError("profile trials use incompatible judge identities")
    scores = {
        key: sum(trial["judge"]["scores"][key] for trial in trials) / count
        for key in SCORE_KEYS
    }
    usages = [trial["judge"]["usage"] for trial in trials]
    judge = {
        **identity,
        "scores": scores,
        "total": sum(trial["judge"]["total"] for trial in trials) / count,
        "latency_seconds": sum(trial["judge"]["latency_seconds"] for trial in trials)
        / count,
        "usage": {
            "calls": count,
            "input_tokens": sum_or_none([usage["input_tokens"] for usage in usages]),
            "output_tokens": sum_or_none([usage["output_tokens"] for usage in usages]),
            "cost_usd": sum_or_none([usage["cost_usd"] for usage in usages]),
        },
    }
    metric_rows = [trial["metrics"] for trial in trials]
    metrics = {
        "input_tokens": sum(row["input_tokens"] for row in metric_rows) / count,
        "output_tokens": sum(row["output_tokens"] for row in metric_rows) / count,
        "tool_calls": sum(row["tool_calls"] for row in metric_rows) / count,
        "agent_wall_seconds": sum(row["agent_wall_seconds"] for row in metric_rows)
        / count,
        "cost_usd": mean_or_none([row["cost_usd"] for row in metric_rows]),
        **summarize_usage(metric_rows, average=True),
    }
    return {
        "trial_count": count,
        "judge": judge,
        "metrics": metrics,
        "trials": list(trials),
    }


TRIM_POLICY = {
    "method": "score_trim_min_max_once",
    "group_by": ["model", "task_id", "profile"],
    "trim_count_per_tail": 1,
    "minimum_samples": 3,
    "insufficient_samples": "retain_all",
    "tie_breaker": "trial_index_then_original_position",
    "passes": 1,
    "selection_metric": "judge.total",
    "aggregation": "equal_weight_task_means",
}


def _mean_metrics(rows: Sequence[dict[str, Any]]) -> dict[str, Any]:
    return {
        **{
            key: mean_or_none([row.get(key) for row in rows])
            for key in (
                "input_tokens",
                "output_tokens",
                "tool_calls",
                "agent_wall_seconds",
                "cost_usd",
            )
        },
        **summarize_usage(rows, average=True),
    }


def profile_statistics(profile: dict[str, Any]) -> dict[str, Any]:
    """Select once by score, then use the same retained trials for every metric.

    Keep the original profile (including all judge calls and trial evidence)
    intact. Legacy summary-only evidence cannot be screened retroactively.
    """
    trials = profile.get("trials")
    if not trials:
        raw = {"judge": profile["judge"]["total"], **profile["metrics"]}
        return {
            "raw": raw,
            "filtered": dict(raw),
            "filter": {
                "status": "unavailable_no_trial_evidence",
                "original_count": None,
                "retained_count": None,
                "excluded_count": None,
                "excluded_trials": [],
            },
        }
    ranked = sorted(
        enumerate(trials, start=1),
        key=lambda item: (
            item[1]["judge"]["total"],
            item[1].get("trial_index", item[0]),
            item[0],
        ),
    )
    trimmed_positions = (
        {ranked[0][0]: "low", ranked[-1][0]: "high"} if len(trials) >= 3 else {}
    )
    retained, excluded = [], []
    retained_indexes = []
    for position, trial in enumerate(trials, start=1):
        score = trial["judge"]["total"]
        trial_index = trial.get("trial_index", position)
        if position not in trimmed_positions:
            retained.append(trial)
            retained_indexes.append(trial_index)
        else:
            excluded.append(
                {
                    "trial_index": trial_index,
                    "trial_name": trial.get("trial_name"),
                    "score": score,
                    "direction": trimmed_positions[position],
                }
            )

    def summarize(rows: Sequence[dict[str, Any]]) -> dict[str, Any]:
        return {
            "judge": sum(row["judge"]["total"] for row in rows) / len(rows),
            **_mean_metrics([row["metrics"] for row in rows]),
        }

    return {
        "raw": summarize(trials),
        "filtered": summarize(retained),
        "filter": {
            "status": "applied"
            if trimmed_positions
            else "unavailable_insufficient_trials",
            "original_count": len(trials),
            "retained_count": len(retained),
            "excluded_count": len(excluded),
            "retained_trial_indexes": retained_indexes,
            "excluded_trials": excluded,
        },
    }


def _comparison(profiles: dict[str, Any]) -> dict[str, Any]:
    baseline, zvec = (profiles[name] for name in PROFILE_NAMES)
    return compare_profiles(baseline, zvec, baseline["judge"], zvec["judge"])


def aggregate_cases(cases: Sequence[dict[str, Any]]) -> dict[str, Any]:
    # Validate every population, including trials later excluded by score.
    compatible_usage_scope(
        [
            row["metrics"]
            for case in cases
            for profile in case["profiles"].values()
            for row in [profile, *profile.get("trials", [])]
        ]
    )
    tasks = [
        {
            "task_id": case["task_id"],
            "profiles": {
                name: profile_statistics(case["profiles"][name])
                for name in PROFILE_NAMES
            },
        }
        for case in cases
    ]
    for task in tasks:
        task["comparison"] = _comparison(
            {name: task["profiles"][name]["filtered"] for name in PROFILE_NAMES}
        )

    def aggregate(view: str) -> dict[str, Any]:
        profiles = {}
        for name in PROFILE_NAMES:
            rows = [task["profiles"][name][view] for task in tasks]
            profiles[name] = {
                "judge": sum(row["judge"] for row in rows) / len(rows),
                **_mean_metrics(rows),
            }
        comparison = _comparison(profiles)
        return {
            "profiles": profiles,
            "comparison": comparison,
            "comparison_samples": {
                key: len(tasks) if value is not None else 0
                for key, value in comparison.items()
            },
        }

    counts = {}
    for name in PROFILE_NAMES:
        filters = [task["profiles"][name]["filter"] for task in tasks]
        counts[name] = {
            key: sum_or_none([row[key] for row in filters])
            for key in ("original_count", "retained_count", "excluded_count")
        }
        counts[name]["unscreened_task_ids"] = [
            task["task_id"]
            for task in tasks
            if task["profiles"][name]["filter"]["status"] != "applied"
        ]

    return {
        **aggregate("filtered"),
        "raw": aggregate("raw"),
        "comparison_basis": "ratio_of_aggregate_profile_means",
        "task_statistics": tasks,
        "filter": {**TRIM_POLICY, "task_count": len(tasks), "profiles": counts},
    }
