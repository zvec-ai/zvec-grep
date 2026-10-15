"""Orchestrate SWE-QA self-judging through separate engine, metric, and report layers."""

from __future__ import annotations

from pathlib import Path
from typing import Any, Sequence

from zg_bench.core.errors import (
    SweQaError,
)
from zg_bench.core.protocol import (
    PROFILE_NAMES,
    SCORE_KEYS,
    judge_label,
)
from zg_bench.engines.judge import (
    Completion,
    default_completion,
    judge_concurrency,
    judge_generation_metadata,
    judge_task_trials,
    judge_temperature,
)
from zg_bench.engines.registry import resolve_opencode_model
from zg_bench.metrics.comparison import (
    case_comparison,
)
from zg_bench.metrics.summary import (
    aggregate_cases,
    summarize_profile,
)
from zg_bench.metrics.usage import (
    compatible_usage_scope,
)
from zg_bench.reports.aggregate import aggregate_reports as aggregate_reports
from zg_bench.reports.render import (
    write_report,
)
from zg_bench.settings import BENCHMARK_SEED
from zg_bench.swe_qa.pairs import (
    load_pairs,
    load_references,
)


def judge_pairs(
    *,
    pairs_root: Path,
    references_path: Path,
    output_dir: Path,
    expected: Sequence[str],
    completion_fn: Completion | None = None,
    attempts: int = 3,
    concurrency: int | None = None,
    model: str = "glm-5.2",
) -> dict[str, Any]:
    """Apply the same-model judge and emit JSON/Markdown reports."""
    label = judge_label(model)
    if attempts < 1 or attempts > 5:
        raise SweQaError("judge attempts must be between 1 and 5")
    concurrency = judge_concurrency(concurrency)
    pairs = load_pairs(pairs_root, expected)
    references = load_references(references_path)
    missing_references = [task for task in expected if task not in references]
    if missing_references:
        raise SweQaError(
            "hard gate is missing reference(s): " + ", ".join(missing_references)
        )

    try:
        runtime = resolve_opencode_model(model, require_credentials=True)
    except ValueError as error:
        raise SweQaError(str(error)) from error
    assert runtime.api_key is not None  # Guaranteed by require_credentials.
    api_key = runtime.api_key
    api_base = runtime.configuration.base_url
    completion_fn = completion_fn or default_completion()

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
        judged_trials = judge_task_trials(
            pair=pair,
            reference=reference,
            completion_fn=completion_fn,
            api_key=api_key,
            api_base=api_base,
            attempts=attempts,
            concurrency=concurrency,
            model=model,
        )
        profile_results: dict[str, dict[str, Any]] = {}
        for profile_name in PROFILE_NAMES:
            trial_results = judged_trials[profile_name]
            for trial_result in trial_results:
                usage = trial_result["judge"]["usage"]
                judge_usage["calls"] += 1
                if usage["input_tokens"] is not None:
                    judge_usage["input_tokens"] += usage["input_tokens"]
                if usage["output_tokens"] is not None:
                    judge_usage["output_tokens"] += usage["output_tokens"]
                if usage["cost_usd"] is None:
                    judge_usage["cost_complete"] = False
                else:
                    judge_usage["cost_usd"] += usage["cost_usd"]
            profile_results[profile_name] = summarize_profile(trial_results)
        case = {
            "task_id": str(reference["task_id"]),
            "role": reference.get("role"),
            "category": reference.get("category"),
            "trial_count": pair["actual_trials"],
            "profiles": profile_results,
        }
        case["comparison"] = case_comparison(case)
        cases.append(case)

    if not judge_usage.pop("cost_complete"):
        judge_usage["cost_usd"] = None
    successful_judgements = judge_usage["calls"]
    report = {
        "schema_version": 3,
        "benchmark": "peng-weihan/SWE-QA-Bench",
        "usage_scope": compatible_usage_scope(
            [
                case["profiles"][profile]["metrics"]
                for case in cases
                for profile in PROFILE_NAMES
            ]
        ),
        "judge": {
            "label": label,
            "model": model,
            "self_judge": True,
            "temperature": judge_temperature(model),
            "seed": BENCHMARK_SEED,
            **judge_generation_metadata(model),
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
        "aggregate": aggregate_cases(cases),
    }
    write_report(report, output_dir)
    return report
