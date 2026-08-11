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
        if not all(isinstance(value, str) and value.strip() for value in (task_id, question, answer)):
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


def _validate_profile(profile: Any, *, task: str, name: str) -> dict[str, Any]:
    if not isinstance(profile, dict):
        raise SweQaError(f"{task} has no valid {name} profile")
    answer = profile.get("answer")
    if not isinstance(answer, str) or not answer.strip():
        raise SweQaError(f"{task} {name} has an empty answer")
    for key in ("input_tokens", "output_tokens", "tool_calls"):
        value = profile.get(key)
        if isinstance(value, bool) or not isinstance(value, int) or value < 0:
            raise SweQaError(f"{task} {name} has invalid {key}")
        if key != "tool_calls" and value == 0:
            raise SweQaError(f"{task} {name} has invalid {key}")
    wall = profile.get("agent_wall_seconds")
    if (
        isinstance(wall, bool)
        or not isinstance(wall, (int, float))
        or not math.isfinite(float(wall))
        or wall <= 0
    ):
        raise SweQaError(f"{task} {name} has invalid agent_wall_seconds")
    cost = profile.get("cost_usd")
    if cost is not None and (
        isinstance(cost, bool)
        or not isinstance(cost, (int, float))
        or not math.isfinite(float(cost))
        or cost < 0
    ):
        raise SweQaError(f"{task} {name} has invalid cost_usd")
    return profile


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
        for name in PROFILE_NAMES:
            _validate_profile(profiles.get(name), task=task_id, name=name)
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
        if isinstance(score, bool) or not isinstance(score, int) or not 1 <= score <= 20:
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
    prompt = _judge_prompt(
        question=question, reference=reference, candidate=candidate
    )
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


def _reduction(baseline: float | int | None, candidate: float | int | None) -> float | None:
    if baseline is None or candidate is None or baseline == 0:
        return None
    return (float(baseline) - float(candidate)) / float(baseline) * 100.0


def _comparison(
    baseline: dict[str, Any], zvec: dict[str, Any], judge_b: int, judge_z: int
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
        "cost_reduction_pct": _reduction(
            baseline["cost_usd"], zvec["cost_usd"]
        ),
    }


def _sum_or_none(values: Sequence[int | float | None]) -> int | float | None:
    if any(value is None for value in values):
        return None
    return sum(value for value in values if value is not None)


def _mean_or_none(values: Sequence[int | float | None]) -> float | None:
    if not values or any(value is None for value in values):
        return None
    return sum(float(value) for value in values if value is not None) / len(values)


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


def _fmt_number(value: int | float, *, decimals: int = 0) -> str:
    if decimals:
        return f"{float(value):,.{decimals}f}"
    return f"{int(value):,}"


def _fmt_delta(value: float | int | None, *, suffix: str = "") -> str:
    if value is None:
        return "N/A"
    return f"{float(value):+.2f}{suffix}"


def _metric_cell(
    baseline: int | float | None,
    zvec: int | float | None,
    reduction: float | None,
    *,
    decimals: int = 0,
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
        "In the Aggregate row, baseline and zvec-grep are raw totals (Judge is the per-case mean), while the third value is the equal-weight arithmetic mean of the per-case deltas or reductions, not a ratio of totals.",
        "",
        "| Case | Judge self-judge | input_token | toolcall | time (s) |",
        "|---|---:|---:|---:|---:|",
    ]
    for case in report["cases"]:
        baseline = case["profiles"]["baseline"]
        zvec = case["profiles"]["zvec-grep"]
        comparison = case["comparison"]
        judge_cell = (
            f"{baseline['judge']['total']:.0f} / {zvec['judge']['total']:.0f} / "
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
                    ),
                    _metric_cell(
                        baseline["metrics"]["tool_calls"],
                        zvec["metrics"]["tool_calls"],
                        comparison["toolcall_reduction_pct"],
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
                ),
                _metric_cell(
                    baseline["tool_calls"],
                    zvec["tool_calls"],
                    comparison["toolcall_reduction_pct"],
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
    api_base = os.environ.get(
        "GLM_BASE_URL", OPENCODE_CUSTOM_GLM_BASE_URL
    ).strip()
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
            judged = _judge_candidate(
                completion_fn=completion_fn,
                api_key=api_key,
                api_base=api_base,
                question=str(reference["question"]),
                reference=str(reference["reference_answer"]),
                candidate=str(profile["answer"]),
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
            profile_results[profile_name] = {
                "judge": judged,
                "metrics": {
                    "input_tokens": profile["input_tokens"],
                    "output_tokens": profile["output_tokens"],
                    "tool_calls": profile["tool_calls"],
                    "agent_wall_seconds": profile["agent_wall_seconds"],
                    "cost_usd": profile["cost_usd"],
                },
            }
        baseline = profile_results["baseline"]
        zvec = profile_results["zvec-grep"]
        cases.append(
            {
                "task_id": str(reference["task_id"]),
                "role": reference.get("role"),
                "category": reference.get("category"),
                "profiles": profile_results,
                "comparison": _comparison(
                    baseline["metrics"],
                    zvec["metrics"],
                    baseline["judge"]["total"],
                    zvec["judge"]["total"],
                ),
            }
        )

    if not judge_usage.pop("cost_complete"):
        judge_usage["cost_usd"] = None
    report = {
        "schema_version": 1,
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
            "successful_judgements": len(cases) * len(PROFILE_NAMES),
            "passed": True,
        },
        "cases": cases,
        "aggregate": _aggregate(cases),
    }
    markdown = _render_report(report)
    output_dir.mkdir(parents=True, exist_ok=True)
    (output_dir / "report.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    (output_dir / "report.md").write_text(markdown, encoding="utf-8")

    summary_path = os.environ.get("GITHUB_STEP_SUMMARY", "").strip()
    if summary_path:
        try:
            with Path(summary_path).open("a", encoding="utf-8") as summary:
                summary.write(markdown)
                if not markdown.endswith("\n"):
                    summary.write("\n")
        except OSError as error:
            raise SweQaError(f"could not append GitHub step summary: {error}") from error
    return report
