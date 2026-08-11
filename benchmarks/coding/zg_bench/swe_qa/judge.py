"""GLM-5.2 self-judge and report generation for SWE-QA pairs."""

from __future__ import annotations

import json
import math
import os
import time
from pathlib import Path
from typing import Any, Callable, Sequence

from ..settings import OPENCODE_CUSTOM_GLM_BASE_URL
from . import SELF_JUDGE_LABEL, SweQaError

SCORE_KEYS = ("correctness", "completeness", "relevance", "clarity", "coherence")
JUDGE_MODEL = "openai/glm-5.2"
PROFILE_NAMES = ("baseline", "zvec-grep")

Completion = Callable[..., Any]


def _load_object(path: Path, *, label: str) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise SweQaError(f"could not read {label} {path}: {error}") from error
    if not isinstance(value, dict):
        raise SweQaError(f"{label} must be a JSON object: {path}")
    return value


def _load_references(path: Path) -> dict[str, dict[str, Any]]:
    root = _load_object(path, label="references")
    records = root.get("references")
    if not isinstance(records, list):
        raise SweQaError("references.references must be an array")
    result: dict[str, dict[str, Any]] = {}
    for index, record in enumerate(records):
        if not isinstance(record, dict):
            raise SweQaError(f"references[{index}] must be an object")
        task_id = record.get("task_id")
        question = record.get("question")
        answer = record.get("reference_answer")
        if not all(
            isinstance(value, str) and value.strip()
            for value in (task_id, question, answer)
        ):
            raise SweQaError(f"references[{index}] has missing judge input")
        if task_id in result:
            raise SweQaError(f"duplicate reference for {task_id}")
        result[task_id] = record
        slug = task_id.replace(":", "-")
        if slug != task_id:
            if slug in result:
                raise SweQaError(f"reference alias collides for {task_id}")
            result[slug] = record
    return result


def _pair_paths(root: Path) -> list[Path]:
    if root.is_file():
        return [root]
    if not root.is_dir():
        raise SweQaError(f"pairs root does not exist: {root}")
    return sorted(
        path
        for path in root.rglob("*.json")
        if path.is_file()
        and (path.name == "pair.json" or path.name.startswith("pair-"))
    )


def _validate_trial(
    trial: Any, *, task: str, profile: str, default_index: int
) -> dict[str, Any]:
    if not isinstance(trial, dict):
        raise SweQaError(f"{task} {profile} trial {default_index} is invalid")
    answer = trial.get("answer")
    if not isinstance(answer, str) or not answer.strip():
        raise SweQaError(f"{task} {profile} trial {default_index} has an empty answer")
    for key in ("input_tokens", "output_tokens", "tool_calls"):
        value = trial.get(key)
        if isinstance(value, bool) or not isinstance(value, int) or value < 0:
            raise SweQaError(
                f"{task} {profile} trial {default_index} has invalid {key}"
            )
        if key != "tool_calls" and value == 0:
            raise SweQaError(
                f"{task} {profile} trial {default_index} has invalid {key}"
            )
    wall = trial.get("agent_wall_seconds")
    if (
        isinstance(wall, bool)
        or not isinstance(wall, (int, float))
        or not math.isfinite(float(wall))
        or wall <= 0
    ):
        raise SweQaError(
            f"{task} {profile} trial {default_index} has invalid "
            "agent_wall_seconds"
        )
    cost = trial.get("cost_usd")
    if cost is not None and (
        isinstance(cost, bool)
        or not isinstance(cost, (int, float))
        or not math.isfinite(float(cost))
        or cost < 0
    ):
        raise SweQaError(
            f"{task} {profile} trial {default_index} has invalid cost_usd"
        )
    trial_index = trial.get("trial_index", default_index)
    if (
        isinstance(trial_index, bool)
        or not isinstance(trial_index, int)
        or trial_index < 1
    ):
        raise SweQaError(f"{task} {profile} has invalid trial_index")
    normalized = dict(trial)
    normalized["trial_index"] = trial_index
    return normalized


def _validate_profile(
    profile: Any, *, task: str, name: str
) -> list[dict[str, Any]]:
    if not isinstance(profile, dict):
        raise SweQaError(f"{task} has no valid {name} profile")
    raw_trials = profile.get("trials")
    if raw_trials is None:
        raw_trials = [profile]
    if not isinstance(raw_trials, list) or not raw_trials:
        raise SweQaError(f"{task} {name} has no trials")
    trials = [
        _validate_trial(
            trial,
            task=task,
            profile=name,
            default_index=index,
        )
        for index, trial in enumerate(raw_trials, start=1)
    ]
    trials.sort(key=lambda trial: trial["trial_index"])
    indexes = [trial["trial_index"] for trial in trials]
    if indexes != list(range(1, len(trials) + 1)):
        raise SweQaError(
            f"{task} {name} trial_index values must be unique and contiguous"
        )
    declared_count = profile.get("trial_count")
    if declared_count is not None and declared_count != len(trials):
        raise SweQaError(f"{task} {name} trial_count does not match trials")
    return trials


def _load_pairs(root: Path, expected: Sequence[str]) -> dict[str, dict[str, Any]]:
    expected_set = set(expected)
    if len(expected_set) != len(expected) or not expected:
        raise SweQaError("expected tasks must be non-empty and unique")
    pairs: dict[str, dict[str, Any]] = {}
    for path in _pair_paths(root):
        pair = _load_object(path, label="pair")
        task_id = pair.get("task_id")
        if not isinstance(task_id, str) or not task_id.strip():
            raise SweQaError(f"pair has no task_id: {path}")
        if task_id not in expected_set:
            continue
        if task_id in pairs:
            raise SweQaError(f"multiple pair artifacts found for {task_id}")
        if pair.get("valid") is not True:
            raise SweQaError(f"pair is not marked valid for {task_id}")
        profiles = pair.get("profiles")
        if not isinstance(profiles, dict):
            raise SweQaError(f"pair has no profiles for {task_id}")
        trial_counts: dict[str, int] = {}
        for name in PROFILE_NAMES:
            profile = profiles.get(name)
            if not isinstance(profile, dict):
                raise SweQaError(f"{task_id} has no valid {name} profile")
            trials = _validate_profile(profile, task=task_id, name=name)
            profile["trials"] = trials
            profile["trial_count"] = len(trials)
            trial_counts[name] = len(trials)
        if len(set(trial_counts.values())) != 1:
            raise SweQaError(f"{task_id} profile trial counts do not match")
        actual_trials = next(iter(trial_counts.values()))
        expected_trials = pair.get("expected_trials", actual_trials)
        declared_actual = pair.get("actual_trials", actual_trials)
        for label, value in (
            ("expected_trials", expected_trials),
            ("actual_trials", declared_actual),
        ):
            if isinstance(value, bool) or not isinstance(value, int) or value < 1:
                raise SweQaError(f"{task_id} has invalid {label}")
        if expected_trials != actual_trials or declared_actual != actual_trials:
            raise SweQaError(f"{task_id} pair trial count does not match profiles")
        pair["expected_trials"] = expected_trials
        pair["actual_trials"] = actual_trials
        pairs[task_id] = pair
    missing = [task for task in expected if task not in pairs]
    if missing:
        raise SweQaError(f"hard gate is missing valid pair(s): {', '.join(missing)}")
    return pairs


def _judge_prompt(*, question: str, reference: str, candidate: str) -> str:
    return f"""You are a strict evaluator. Score the candidate only against the supplied question and reference answer.

Score each dimension as an integer from 1 through 20:
- correctness: factual agreement with the reference; penalize errors.
- completeness: coverage of the reference's important points; penalize omissions.
- relevance: focus on the question; penalize tangents.
- clarity: precision and ease of understanding.
- coherence: logical organization and consistency of the explanation.

Scores 16-20 are reserved for excellent answers. When uncertain, choose the lower score. Treat the reference as judge-only evidence, not text to reproduce.

Question:
{question}

Reference answer:
{reference}

Candidate answer:
{candidate}

Return only one strict JSON object with exactly these five integer fields and no markdown:
{{"correctness": 1, "completeness": 1, "relevance": 1, "clarity": 1, "coherence": 1}}
"""


def _response_mapping(response: Any) -> dict[str, Any]:
    if isinstance(response, dict):
        return response
    if hasattr(response, "model_dump"):
        value = response.model_dump()
        if isinstance(value, dict):
            return value
    try:
        value = dict(response)
    except (TypeError, ValueError) as error:
        raise SweQaError("judge returned an unsupported response object") from error
    return value


def _response_content(response: Any) -> str:
    root = _response_mapping(response)
    choices = root.get("choices")
    if not isinstance(choices, list) or not choices or not isinstance(choices[0], dict):
        raise SweQaError("judge response has no choices")
    message = choices[0].get("message")
    if not isinstance(message, dict) or not isinstance(message.get("content"), str):
        raise SweQaError("judge response has no text content")
    return message["content"]


def _parse_scores(content: str) -> dict[str, int]:
    try:
        value = json.loads(content)
    except json.JSONDecodeError as error:
        raise SweQaError("judge response is not strict JSON") from error
    if not isinstance(value, dict) or set(value) != set(SCORE_KEYS):
        raise SweQaError("judge response does not have the five rubric fields")
    scores: dict[str, int] = {}
    for key in SCORE_KEYS:
        score = value.get(key)
        if (
            isinstance(score, bool)
            or not isinstance(score, int)
            or not 1 <= score <= 20
        ):
            raise SweQaError(f"judge response has invalid {key} score")
        scores[key] = score
    return scores


def _response_usage(response: Any) -> dict[str, int | float | None]:
    root = _response_mapping(response)
    usage = root.get("usage")
    if not isinstance(usage, dict):
        usage = {}
    hidden = root.get("_hidden_params")
    if not isinstance(hidden, dict):
        hidden = getattr(response, "_hidden_params", {})
    if not isinstance(hidden, dict):
        hidden = {}

    def token(name: str) -> int | None:
        value = usage.get(name)
        return value if isinstance(value, int) and not isinstance(value, bool) else None

    cost = hidden.get("response_cost")
    if isinstance(cost, bool) or not isinstance(cost, (int, float)):
        cost = None
    return {
        "input_tokens": token("prompt_tokens"),
        "output_tokens": token("completion_tokens"),
        "cost_usd": cost,
    }


def _judge_candidate(
    *,
    completion_fn: Completion,
    api_key: str,
    api_base: str,
    question: str,
    reference: str,
    candidate: str,
    attempts: int,
) -> dict[str, Any]:
    prompt = _judge_prompt(question=question, reference=reference, candidate=candidate)
    last_failure = "unknown"
    for attempt in range(1, attempts + 1):
        started = time.monotonic()
        try:
            response = completion_fn(
                model=JUDGE_MODEL,
                api_key=api_key,
                api_base=api_base,
                temperature=0,
                messages=[{"role": "user", "content": prompt}],
                response_format={"type": "json_object"},
                extra_body={"enable_thinking": False},
            )
        except Exception as error:  # Provider errors have no shared stable base.
            last_failure = f"transport error ({type(error).__name__})"
        else:
            try:
                scores = _parse_scores(_response_content(response))
            except SweQaError as error:
                last_failure = str(error)
            else:
                return {
                    "label": SELF_JUDGE_LABEL,
                    "model": "glm-5.2",
                    "scores": scores,
                    "total": sum(scores.values()),
                    "latency_seconds": time.monotonic() - started,
                    "usage": _response_usage(response),
                }
        if attempt < attempts:
            time.sleep(min(float(attempt), 2.0))
    raise SweQaError(f"judge failed after {attempts} attempts: {last_failure}")


def _reduction(
    baseline: float | int | None, candidate: float | int | None
) -> float | None:
    if baseline is None or candidate is None or baseline == 0:
        return None
    return (float(baseline) - float(candidate)) / float(baseline) * 100.0


def _comparison(
    baseline: dict[str, Any],
    zvec: dict[str, Any],
    judge_b: int | float,
    judge_z: int | float,
) -> dict[str, Any]:
    return {
        "judge_delta": judge_z - judge_b,
        "input_token_reduction_pct": _reduction(
            baseline["input_tokens"], zvec["input_tokens"]
        ),
        "toolcall_reduction_pct": _reduction(
            baseline["tool_calls"], zvec["tool_calls"]
        ),
        "time_reduction_pct": _reduction(
            baseline["agent_wall_seconds"], zvec["agent_wall_seconds"]
        ),
        "cost_reduction_pct": _reduction(baseline["cost_usd"], zvec["cost_usd"]),
    }


def _sum_or_none(values: Sequence[int | float | None]) -> int | float | None:
    if any(value is None for value in values):
        return None
    return sum(value for value in values if value is not None)


def _mean_or_none(values: Sequence[int | float | None]) -> float | None:
    if not values or any(value is None for value in values):
        return None
    return sum(float(value) for value in values if value is not None) / len(values)


def _summarize_profile(trials: Sequence[dict[str, Any]]) -> dict[str, Any]:
    count = len(trials)
    scores = {
        key: sum(trial["judge"]["scores"][key] for trial in trials) / count
        for key in SCORE_KEYS
    }
    usages = [trial["judge"]["usage"] for trial in trials]
    judge = {
        "label": SELF_JUDGE_LABEL,
        "model": "glm-5.2",
        "scores": scores,
        "total": sum(trial["judge"]["total"] for trial in trials) / count,
        "latency_seconds": sum(
            trial["judge"]["latency_seconds"] for trial in trials
        )
        / count,
        "usage": {
            "calls": count,
            "input_tokens": _sum_or_none(
                [usage["input_tokens"] for usage in usages]
            ),
            "output_tokens": _sum_or_none(
                [usage["output_tokens"] for usage in usages]
            ),
            "cost_usd": _sum_or_none([usage["cost_usd"] for usage in usages]),
        },
    }
    metric_rows = [trial["metrics"] for trial in trials]
    metrics = {
        "input_tokens": sum(row["input_tokens"] for row in metric_rows) / count,
        "output_tokens": sum(row["output_tokens"] for row in metric_rows) / count,
        "tool_calls": sum(row["tool_calls"] for row in metric_rows) / count,
        "agent_wall_seconds": sum(
            row["agent_wall_seconds"] for row in metric_rows
        )
        / count,
        "cost_usd": _mean_or_none([row["cost_usd"] for row in metric_rows]),
    }
    return {
        "trial_count": count,
        "judge": judge,
        "metrics": metrics,
        "trials": list(trials),
    }


def _paired_comparison(
    baseline_trials: Sequence[dict[str, Any]],
    zvec_trials: Sequence[dict[str, Any]],
) -> dict[str, Any]:
    if len(baseline_trials) != len(zvec_trials):
        raise SweQaError("profile trial counts do not match")
    trial_rows: list[dict[str, Any]] = []
    for baseline, zvec in zip(baseline_trials, zvec_trials, strict=True):
        baseline_index = baseline["trial_index"]
        zvec_index = zvec["trial_index"]
        if baseline_index != zvec_index:
            raise SweQaError("profile trial_index values do not match")
        trial_rows.append(
            {
                "trial_index": baseline_index,
                **_comparison(
                    baseline["metrics"],
                    zvec["metrics"],
                    baseline["judge"]["total"],
                    zvec["judge"]["total"],
                ),
            }
        )
    comparison_keys = (
        "judge_delta",
        "input_token_reduction_pct",
        "toolcall_reduction_pct",
        "time_reduction_pct",
        "cost_reduction_pct",
    )
    return {
        **{
            key: _mean_or_none([row[key] for row in trial_rows])
            for key in comparison_keys
        },
        "trials": trial_rows,
    }


def _aggregate(cases: Sequence[dict[str, Any]]) -> dict[str, Any]:
    count = len(cases)
    profiles: dict[str, dict[str, Any]] = {}
    for profile in PROFILE_NAMES:
        profile_rows = [case["profiles"][profile] for case in cases]
        profiles[profile] = {
            "judge": sum(row["judge"]["total"] for row in profile_rows) / count,
            "input_tokens": sum(row["metrics"]["input_tokens"] for row in profile_rows),
            "tool_calls": sum(row["metrics"]["tool_calls"] for row in profile_rows),
            "agent_wall_seconds": sum(
                row["metrics"]["agent_wall_seconds"] for row in profile_rows
            ),
            "cost_usd": _sum_or_none(
                [row["metrics"]["cost_usd"] for row in profile_rows]
            ),
        }
    comparison_keys = (
        "judge_delta",
        "input_token_reduction_pct",
        "toolcall_reduction_pct",
        "time_reduction_pct",
        "cost_reduction_pct",
    )
    comparison = {
        key: _mean_or_none([case["comparison"][key] for case in cases])
        for key in comparison_keys
    }
    return {"profiles": profiles, "comparison": comparison}


def _fmt_number(value: int | float, *, decimals: int = 2) -> str:
    return f"{float(value):,.{decimals}f}"


def _fmt_delta(value: float | int | None, *, suffix: str = "") -> str:
    if value is None:
        return "N/A"
    return f"{float(value):+.2f}{suffix}"


def _metric_cell(
    baseline: int | float | None,
    zvec: int | float | None,
    reduction: float | None,
    *,
    decimals: int = 2,
) -> str:
    if baseline is None or zvec is None:
        return "N/A"
    left = _fmt_number(baseline, decimals=decimals)
    right = _fmt_number(zvec, decimals=decimals)
    return f"{left} / {right} / {_fmt_delta(reduction, suffix='%')}"


def _render_report(report: dict[str, Any]) -> str:
    lines = [
        "# SWE-QA-Bench manual CI report",
        "",
        f"Judge: **{SELF_JUDGE_LABEL}** (GLM-5.2 self-judge).",
        "",
        "This run is **report-only**. Numeric scores and deltas are not code-review or merge gates. The hard gate only requires every expected pair and every judge call to succeed.",
        "",
        "All cells use `baseline / zvec-grep / delta-or-reduction`. Positive reduction means zvec-grep used less.",
        "",
        "Each case's baseline and zvec-grep values are arithmetic means across that profile's trials. Its third value is the equal-weight arithmetic mean of deltas or reductions after pairing sorted `trial_index` values; it is not a ratio of profile means.",
        "",
        "In the Aggregate row, baseline and zvec-grep efficiency values are sums of the per-case trial means (Judge is the equal-weight per-case mean), while the third value is the equal-weight arithmetic mean of the per-case deltas or reductions, not a ratio of totals.",
        "",
        "| Case | Judge self-judge | input_token | toolcall | time (s) |",
        "|---|---:|---:|---:|---:|",
    ]
    for case in report["cases"]:
        baseline = case["profiles"]["baseline"]
        zvec = case["profiles"]["zvec-grep"]
        comparison = case["comparison"]
        judge_cell = (
            f"{baseline['judge']['total']:.2f} / {zvec['judge']['total']:.2f} / "
            f"{_fmt_delta(comparison['judge_delta'])}"
        )
        lines.append(
            "| "
            + " | ".join(
                (
                    str(case["task_id"]),
                    judge_cell,
                    _metric_cell(
                        baseline["metrics"]["input_tokens"],
                        zvec["metrics"]["input_tokens"],
                        comparison["input_token_reduction_pct"],
                        decimals=2,
                    ),
                    _metric_cell(
                        baseline["metrics"]["tool_calls"],
                        zvec["metrics"]["tool_calls"],
                        comparison["toolcall_reduction_pct"],
                        decimals=2,
                    ),
                    _metric_cell(
                        baseline["metrics"]["agent_wall_seconds"],
                        zvec["metrics"]["agent_wall_seconds"],
                        comparison["time_reduction_pct"],
                        decimals=2,
                    ),
                )
            )
            + " |"
        )

    aggregate = report["aggregate"]
    baseline = aggregate["profiles"]["baseline"]
    zvec = aggregate["profiles"]["zvec-grep"]
    comparison = aggregate["comparison"]
    judge_cell = (
        f"{baseline['judge']:.2f} / {zvec['judge']:.2f} / "
        f"{_fmt_delta(comparison['judge_delta'])}"
    )
    lines.append(
        "| "
        + " | ".join(
            (
                "**Aggregate**",
                judge_cell,
                _metric_cell(
                    baseline["input_tokens"],
                    zvec["input_tokens"],
                    comparison["input_token_reduction_pct"],
                    decimals=2,
                ),
                _metric_cell(
                    baseline["tool_calls"],
                    zvec["tool_calls"],
                    comparison["toolcall_reduction_pct"],
                    decimals=2,
                ),
                _metric_cell(
                    baseline["agent_wall_seconds"],
                    zvec["agent_wall_seconds"],
                    comparison["time_reduction_pct"],
                    decimals=2,
                ),
            )
        )
        + " |"
    )
    lines.append("")
    return "\n".join(lines)


def _write_report(report: dict[str, Any], output_dir: Path) -> None:
    markdown = _render_report(report)
    try:
        output_dir.mkdir(parents=True, exist_ok=True)
        (output_dir / "report.json").write_text(
            json.dumps(report, ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8",
        )
        (output_dir / "report.md").write_text(markdown, encoding="utf-8")
    except OSError as error:
        raise SweQaError(f"could not write SWE-QA report: {error}") from error

    summary_path = os.environ.get("GITHUB_STEP_SUMMARY", "").strip()
    if summary_path:
        try:
            with Path(summary_path).open("a", encoding="utf-8") as summary:
                summary.write(markdown)
                if not markdown.endswith("\n"):
                    summary.write("\n")
        except OSError as error:
            raise SweQaError(
                f"could not append GitHub step summary: {error}"
            ) from error


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


def _valid_number(value: Any, *, allow_none: bool = False) -> bool:
    if value is None:
        return allow_none
    return (
        not isinstance(value, bool)
        and isinstance(value, (int, float))
        and math.isfinite(float(value))
    )


def _validate_report_judge(value: Any, *, prefix: str) -> None:
    if not isinstance(value, dict):
        raise SweQaError(f"{prefix}: missing judge result")
    total = value.get("total")
    if not _valid_number(total) or not 5 <= float(total) <= 100:
        raise SweQaError(f"{prefix}: invalid judge total")
    scores = value.get("scores")
    if scores is not None:
        if not isinstance(scores, dict) or set(scores) != set(SCORE_KEYS):
            raise SweQaError(f"{prefix}: invalid judge scores")
        if any(
            not _valid_number(scores.get(key))
            or not 1 <= float(scores[key]) <= 20
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


def _report_profile_trial_count(profile: dict[str, Any], *, prefix: str) -> int:
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
        _validate_report_judge(trial.get("judge"), prefix=trial_prefix)
        _validate_report_metrics(trial.get("metrics"), prefix=trial_prefix)
    if sorted(indexes) != list(range(1, len(raw_trials) + 1)):
        raise SweQaError(f"{prefix}: trial_index values are not contiguous")
    declared = profile.get("trial_count", len(raw_trials))
    if declared != len(raw_trials):
        raise SweQaError(f"{prefix}: trial_count does not match evidence")
    return len(raw_trials)


def _case_judgement_count(case: dict[str, Any]) -> int:
    return sum(
        len(case["profiles"][name]["trials"])
        if "trials" in case["profiles"][name]
        else 1
        for name in PROFILE_NAMES
    )


def _validate_task_report(report: dict[str, Any], path: Path) -> dict[str, Any]:
    prefix = f"invalid per-task report {path}"
    if report.get("schema_version") not in (1, 2):
        raise SweQaError(f"{prefix}: unsupported schema_version")
    if report.get("benchmark") != "peng-weihan/SWE-QA-Bench":
        raise SweQaError(f"{prefix}: unexpected benchmark")

    judge = report.get("judge")
    if not isinstance(judge, dict) or (
        judge.get("label") != SELF_JUDGE_LABEL
        or judge.get("model") != "glm-5.2"
        or judge.get("self_judge") is not True
        or judge.get("temperature") != 0
        or judge.get("rubric") != list(SCORE_KEYS)
    ):
        raise SweQaError(f"{prefix}: incompatible judge metadata")
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
        _validate_report_judge(profile.get("judge"), prefix=profile_prefix)
        _validate_report_metrics(profile.get("metrics"), prefix=profile_prefix)
        trial_count = _report_profile_trial_count(
            profile, prefix=profile_prefix
        )
        trial_counts.append(trial_count)
        judgement_count += trial_count

    if len(set(trial_counts)) != 1:
        raise SweQaError(f"{prefix}: profile trial counts do not match")
    declared_case_count = case.get("trial_count", trial_counts[0])
    if declared_case_count != trial_counts[0]:
        raise SweQaError(f"{prefix}: case trial_count does not match evidence")

    if usage["calls"] != judgement_count:
        raise SweQaError(f"{prefix}: judge usage calls do not match trial evidence")
    if gate.get("successful_judgements") != judgement_count:
        raise SweQaError(f"{prefix}: gate count does not match trial evidence")

    comparison = case.get("comparison")
    comparison_keys = (
        "judge_delta",
        "input_token_reduction_pct",
        "toolcall_reduction_pct",
        "time_reduction_pct",
        "cost_reduction_pct",
    )
    if not isinstance(comparison, dict) or any(
        not _valid_number(comparison.get(key), allow_none=key != "judge_delta")
        for key in comparison_keys
    ):
        raise SweQaError(f"{prefix}: invalid case comparison")
    return case


def _combined_judge(reports: Sequence[dict[str, Any]]) -> dict[str, Any]:
    first = reports[0]["judge"]
    metadata_keys = ("label", "model", "self_judge", "temperature", "rubric")
    metadata = {key: first[key] for key in metadata_keys}
    for report in reports[1:]:
        judge = report["judge"]
        if any(judge.get(key) != metadata[key] for key in metadata_keys):
            raise SweQaError("per-task reports use incompatible judge metadata")

    usages = [report["judge"]["usage"] for report in reports]
    metadata["usage"] = {
        "calls": sum(usage["calls"] for usage in usages),
        "input_tokens": sum(usage["input_tokens"] for usage in usages),
        "output_tokens": sum(usage["output_tokens"] for usage in usages),
        "cost_usd": _sum_or_none([usage["cost_usd"] for usage in usages]),
    }
    return metadata


def aggregate_reports(*, reports_root: Path, output_dir: Path) -> dict[str, Any]:
    """Combine successful one-task reports without making any model calls."""
    source_reports: list[dict[str, Any]] = []
    cases: list[dict[str, Any]] = []
    task_sources: dict[str, Path] = {}
    for path in _report_paths(reports_root, output_dir):
        source = _load_object(path, label="per-task report")
        case = _validate_task_report(source, path)
        task_id = case["task_id"]
        if task_id in task_sources:
            raise SweQaError(
                f"duplicate task report for {task_id}: "
                f"{task_sources[task_id]} and {path}"
            )
        task_sources[task_id] = path
        source_reports.append(source)
        cases.append(case)

    cases.sort(key=lambda case: case["task_id"])
    task_ids = [case["task_id"] for case in cases]
    successful_judgements = sum(_case_judgement_count(case) for case in cases)
    report = {
        "schema_version": 2,
        "benchmark": "peng-weihan/SWE-QA-Bench",
        "judge": _combined_judge(source_reports),
        "gate": {
            "kind": "completion-only",
            "report_only": True,
            "numeric_thresholds": False,
            "expected_tasks": task_ids,
            "valid_pairs": len(cases),
            "successful_judgements": successful_judgements,
            "passed": True,
        },
        "cases": cases,
        "aggregate": _aggregate(cases),
    }
    _write_report(report, output_dir)
    return report


def _default_completion() -> Completion:
    try:
        import litellm
    except ImportError as error:
        raise SweQaError("LiteLLM is required to run the SWE-QA judge") from error
    litellm.suppress_debug_info = True
    return litellm.completion


def judge_pairs(
    *,
    pairs_root: Path,
    references_path: Path,
    output_dir: Path,
    expected: Sequence[str],
    completion_fn: Completion | None = None,
    attempts: int = 3,
) -> dict[str, Any]:
    """Apply the same-model judge and emit JSON/Markdown reports."""
    if attempts < 1 or attempts > 5:
        raise SweQaError("judge attempts must be between 1 and 5")
    pairs = _load_pairs(pairs_root, expected)
    references = _load_references(references_path)
    missing_references = [task for task in expected if task not in references]
    if missing_references:
        raise SweQaError(
            "hard gate is missing reference(s): " + ", ".join(missing_references)
        )

    api_key = os.environ.get("GLM_API_KEY", "").strip()
    if not api_key:
        raise SweQaError("GLM_API_KEY is required for the self-judge")
    api_base = os.environ.get("GLM_BASE_URL", OPENCODE_CUSTOM_GLM_BASE_URL).strip()
    if not api_base:
        raise SweQaError("GLM_BASE_URL must not be empty")
    completion_fn = completion_fn or _default_completion()

    cases: list[dict[str, Any]] = []
    judge_usage = {
        "calls": 0,
        "input_tokens": 0,
        "output_tokens": 0,
        "cost_usd": 0.0,
        "cost_complete": True,
    }
    for task in expected:
        pair = pairs[task]
        reference = references[task]
        profile_results: dict[str, dict[str, Any]] = {}
        for profile_name in PROFILE_NAMES:
            profile = pair["profiles"][profile_name]
            trial_results: list[dict[str, Any]] = []
            for trial in profile["trials"]:
                judged = _judge_candidate(
                    completion_fn=completion_fn,
                    api_key=api_key,
                    api_base=api_base,
                    question=str(reference["question"]),
                    reference=str(reference["reference_answer"]),
                    candidate=str(trial["answer"]),
                    attempts=attempts,
                )
                usage = judged["usage"]
                judge_usage["calls"] += 1
                if usage["input_tokens"] is not None:
                    judge_usage["input_tokens"] += usage["input_tokens"]
                if usage["output_tokens"] is not None:
                    judge_usage["output_tokens"] += usage["output_tokens"]
                if usage["cost_usd"] is None:
                    judge_usage["cost_complete"] = False
                else:
                    judge_usage["cost_usd"] += usage["cost_usd"]
                trial_results.append(
                    {
                        "trial_index": trial["trial_index"],
                        "trial_name": trial.get("trial_name"),
                        "judge": judged,
                        "metrics": {
                            "input_tokens": trial["input_tokens"],
                            "output_tokens": trial["output_tokens"],
                            "tool_calls": trial["tool_calls"],
                            "agent_wall_seconds": trial["agent_wall_seconds"],
                            "cost_usd": trial["cost_usd"],
                        },
                    }
                )
            profile_results[profile_name] = _summarize_profile(trial_results)
        baseline_trials = profile_results["baseline"]["trials"]
        zvec_trials = profile_results["zvec-grep"]["trials"]
        cases.append(
            {
                "task_id": str(reference["task_id"]),
                "role": reference.get("role"),
                "category": reference.get("category"),
                "trial_count": pair["actual_trials"],
                "profiles": profile_results,
                "comparison": _paired_comparison(
                    baseline_trials,
                    zvec_trials,
                ),
            }
        )

    if not judge_usage.pop("cost_complete"):
        judge_usage["cost_usd"] = None
    successful_judgements = judge_usage["calls"]
    report = {
        "schema_version": 2,
        "benchmark": "peng-weihan/SWE-QA-Bench",
        "judge": {
            "label": SELF_JUDGE_LABEL,
            "model": "glm-5.2",
            "self_judge": True,
            "temperature": 0,
            "rubric": list(SCORE_KEYS),
            "usage": judge_usage,
        },
        "gate": {
            "kind": "completion-only",
            "report_only": True,
            "numeric_thresholds": False,
            "expected_tasks": list(expected),
            "valid_pairs": len(cases),
            "successful_judgements": successful_judgements,
            "passed": True,
        },
        "cases": cases,
        "aggregate": _aggregate(cases),
    }
    _write_report(report, output_dir)
    return report
