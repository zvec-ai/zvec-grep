"""Validate per-task report evidence before it enters aggregation."""

from __future__ import annotations

import math
from pathlib import Path
from typing import Any

from zg_bench.core.errors import SweQaError
from zg_bench.core.protocol import (
    COMPARISON_KEYS,
    JUDGE_GENERATION_METADATA_KEYS,
    JUDGE_MODELS,
    PROFILE_NAMES,
    SCORE_KEYS,
    judge_label,
)
from zg_bench.metrics.comparison import (
    case_comparison,
)
from zg_bench.metrics.numbers import (
    same_metric,
)
from zg_bench.metrics.usage import (
    LEGACY_USAGE_SCOPE,
    SESSION_USAGE_METRICS,
    SESSION_USAGE_SCOPE,
    compatible_usage_scope,
    summarize_usage,
    validate_usage_metrics,
)


def _validate_judge_generation_metadata(judge: dict[str, Any], prefix: str) -> None:
    present = [key in judge for key in JUDGE_GENERATION_METADATA_KEYS]
    if not any(present):
        return  # Legacy reports remain readable, but cannot mix with new ones.
    effort = judge.get("reasoning_effort")
    tokens = judge.get("max_tokens")
    if (
        not all(present)
        or not isinstance(judge.get("enable_thinking"), bool)
        or (effort is not None and (not isinstance(effort, str) or not effort.strip()))
        or isinstance(tokens, bool)
        or not isinstance(tokens, int)
        or tokens <= 0
        or judge.get("response_format") not in (None, {"type": "json_object"})
    ):
        raise SweQaError(f"{prefix}: invalid judge generation metadata")


def _valid_number(value: Any, *, allow_none: bool = False) -> bool:
    if value is None:
        return allow_none
    return (
        not isinstance(value, bool)
        and isinstance(value, (int, float))
        and math.isfinite(float(value))
    )


def _validate_report_judge(
    value: Any, *, prefix: str, expected_model: str | None = None
) -> None:
    if not isinstance(value, dict):
        raise SweQaError(f"{prefix}: missing judge result")
    # Some legacy evidence omitted trial identities. If present, both fields
    # must agree with the report identity instead of silently mixing models.
    if "model" in value or "label" in value:
        model = value.get("model")
        if (
            model not in JUDGE_MODELS
            or value.get("label") != judge_label(model)
            or (expected_model is not None and model != expected_model)
        ):
            raise SweQaError(f"{prefix}: incompatible judge identity")
    total = value.get("total")
    if not _valid_number(total) or not 5 <= float(total) <= 100:
        raise SweQaError(f"{prefix}: invalid judge total")
    scores = value.get("scores")
    if scores is not None:
        if not isinstance(scores, dict) or set(scores) != set(SCORE_KEYS):
            raise SweQaError(f"{prefix}: invalid judge scores")
        if any(
            not _valid_number(scores.get(key)) or not 1 <= float(scores[key]) <= 20
            for key in SCORE_KEYS
        ):
            raise SweQaError(f"{prefix}: invalid judge scores")


def _validate_report_metrics(value: Any, *, prefix: str) -> None:
    if not isinstance(value, dict):
        raise SweQaError(f"{prefix}: invalid metrics")
    for key in ("input_tokens", "output_tokens", "tool_calls"):
        metric = value.get(key)
        if not _valid_number(metric) or float(metric) < 0:
            raise SweQaError(f"{prefix}: invalid {key}")
    wall = value.get("agent_wall_seconds")
    if not _valid_number(wall) or float(wall) <= 0:
        raise SweQaError(f"{prefix}: invalid agent_wall_seconds")
    cost = value.get("cost_usd")
    if not _valid_number(cost, allow_none=True) or (
        cost is not None and float(cost) < 0
    ):
        raise SweQaError(f"{prefix}: invalid cost_usd")
    value["usage_scope"] = validate_usage_metrics(value, integer=False)


def _report_profile_trial_count(
    profile: dict[str, Any], *, prefix: str, expected_model: str | None = None
) -> int:
    raw_trials = profile.get("trials")
    if raw_trials is None:
        return 1
    if not isinstance(raw_trials, list) or not raw_trials:
        raise SweQaError(f"{prefix}: no trial evidence")
    indexes: list[int] = []
    for position, trial in enumerate(raw_trials, start=1):
        if not isinstance(trial, dict):
            raise SweQaError(f"{prefix}: invalid trial evidence")
        trial_index = trial.get("trial_index", position)
        if (
            isinstance(trial_index, bool)
            or not isinstance(trial_index, int)
            or trial_index < 1
        ):
            raise SweQaError(f"{prefix}: invalid trial_index")
        indexes.append(trial_index)
        trial_prefix = f"{prefix} trial {trial_index}"
        _validate_report_judge(
            trial.get("judge"), prefix=trial_prefix, expected_model=expected_model
        )
        _validate_report_metrics(trial.get("metrics"), prefix=trial_prefix)
    if sorted(indexes) != list(range(1, len(raw_trials) + 1)):
        raise SweQaError(f"{prefix}: trial_index values are not contiguous")
    declared = profile.get("trial_count", len(raw_trials))
    if declared != len(raw_trials):
        raise SweQaError(f"{prefix}: trial_count does not match evidence")
    scope = compatible_usage_scope(
        [profile["metrics"], *[trial["metrics"] for trial in raw_trials]]
    )
    if scope == SESSION_USAGE_SCOPE:
        rows = [trial["metrics"] for trial in raw_trials]
        expected = summarize_usage(rows, average=True)
        expected["agent_wall_seconds"] = sum(
            row["agent_wall_seconds"] for row in rows
        ) / len(rows)
        for key in (*SESSION_USAGE_METRICS, "agent_wall_seconds"):
            if not same_metric(profile["metrics"][key], expected[key]):
                raise SweQaError(f"{prefix}: profile mean {key} disagrees with trials")
        for part in ("root", "descendants", "total"):
            for key in SESSION_USAGE_METRICS:
                if not same_metric(
                    profile["metrics"]["session_usage"][part][key],
                    expected["session_usage"][part][key],
                ):
                    raise SweQaError(
                        f"{prefix}: profile session split disagrees with trials"
                    )
    return len(raw_trials)


def validate_task_report(report: dict[str, Any], path: Path) -> dict[str, Any]:
    prefix = f"invalid per-task report {path}"
    if report.get("schema_version") not in (1, 2, 3, 4):
        raise SweQaError(f"{prefix}: unsupported schema_version")
    if report.get("benchmark") != "peng-weihan/SWE-QA-Bench":
        raise SweQaError(f"{prefix}: unexpected benchmark")

    judge = report.get("judge")
    if not isinstance(judge, dict) or (
        judge.get("model") not in JUDGE_MODELS
        or judge.get("label") != judge_label(judge["model"])
        or judge.get("self_judge") is not True
        or not _valid_number(judge.get("temperature"))
        or judge["temperature"] < 0
        or judge.get("rubric") != list(SCORE_KEYS)
    ):
        raise SweQaError(f"{prefix}: incompatible judge metadata")
    if "seed" in judge and (
        isinstance(judge["seed"], bool) or not isinstance(judge["seed"], int)
    ):
        raise SweQaError(f"{prefix}: invalid judge seed")
    _validate_judge_generation_metadata(judge, prefix)
    usage = judge.get("usage")
    if not isinstance(usage, dict):
        raise SweQaError(f"{prefix}: missing judge usage")
    for key in ("calls", "input_tokens", "output_tokens"):
        value = usage.get(key)
        if isinstance(value, bool) or not isinstance(value, int) or value < 0:
            raise SweQaError(f"{prefix}: invalid judge usage {key}")
    cost = usage.get("cost_usd")
    if not _valid_number(cost, allow_none=True) or (
        cost is not None and float(cost) < 0
    ):
        raise SweQaError(f"{prefix}: invalid judge usage cost_usd")

    gate = report.get("gate")
    if not isinstance(gate, dict) or gate.get("passed") is not True:
        raise SweQaError(f"{prefix}: judge gate did not pass")
    if gate.get("report_only") is not True:
        raise SweQaError(f"{prefix}: source is not report-only")

    cases = report.get("cases")
    if not isinstance(cases, list) or len(cases) != 1:
        raise SweQaError(f"{prefix}: expected exactly one judged case")
    case = cases[0]
    if not isinstance(case, dict):
        raise SweQaError(f"{prefix}: case must be an object")
    task_id = case.get("task_id")
    if not isinstance(task_id, str) or not task_id.strip():
        raise SweQaError(f"{prefix}: case has no task_id")

    profiles = case.get("profiles")
    if not isinstance(profiles, dict):
        raise SweQaError(f"{prefix}: case has no profiles")
    judgement_count = 0
    trial_counts: list[int] = []
    for profile_name in PROFILE_NAMES:
        profile = profiles.get(profile_name)
        if not isinstance(profile, dict):
            raise SweQaError(f"{prefix}: case has no {profile_name} profile")
        profile_prefix = f"{prefix} {profile_name}"
        _validate_report_judge(
            profile.get("judge"), prefix=profile_prefix, expected_model=judge["model"]
        )
        _validate_report_metrics(profile.get("metrics"), prefix=profile_prefix)
        trial_count = _report_profile_trial_count(
            profile, prefix=profile_prefix, expected_model=judge["model"]
        )
        trial_counts.append(trial_count)
        judgement_count += trial_count

    if len(set(trial_counts)) != 1:
        raise SweQaError(f"{prefix}: profile trial counts do not match")
    usage_scope = compatible_usage_scope(
        [profiles[name]["metrics"] for name in PROFILE_NAMES]
    )
    if report.get("usage_scope", LEGACY_USAGE_SCOPE) != usage_scope:
        raise SweQaError(f"{prefix}: report usage_scope disagrees with metrics")
    declared_case_count = case.get("trial_count", trial_counts[0])
    if declared_case_count != trial_counts[0]:
        raise SweQaError(f"{prefix}: case trial_count does not match evidence")

    if usage["calls"] != judgement_count:
        raise SweQaError(f"{prefix}: judge usage calls do not match trial evidence")
    if gate.get("successful_judgements") != judgement_count:
        raise SweQaError(f"{prefix}: gate count does not match trial evidence")

    comparison = case.get("comparison")
    if not isinstance(comparison, dict) or any(
        not _valid_number(comparison.get(key), allow_none=key != "judge_delta")
        for key in COMPARISON_KEYS
    ):
        raise SweQaError(f"{prefix}: invalid case comparison")
    # Recompute instead of trusting a potentially stale task summary produced
    # with a different trial-to-task comparison rule.
    case["comparison"] = case_comparison(case)
    return case
