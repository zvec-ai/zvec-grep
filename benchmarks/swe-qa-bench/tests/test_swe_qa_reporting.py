from __future__ import annotations

import json
import copy
import shutil
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from unittest.mock import patch

from zg_bench.settings import (
    OPENCODE_QWEN_ENABLE_THINKING,
    OPENCODE_QWEN_REASONING_EFFORT,
    OPENCODE_QWEN_TEMPERATURE,
)
from zg_bench.swe_qa import SELF_JUDGE_LABEL, SweQaError
from zg_bench.swe_qa.cli import main as swe_qa_main
from zg_bench.swe_qa.collect import SESSION_USAGE_METRICS, SESSION_USAGE_SCOPE, collect_pair
from zg_bench.swe_qa.judge import (
    MAX_JUDGE_CONCURRENCY,
    _aggregate,
    _default_completion,
    _judge_candidate,
    _metric_cell,
    aggregate_reports,
    judge_pairs,
)
from zg_bench.swe_qa.validation import validate_assets

SWE_QA_BENCH_DIR = Path(__file__).resolve().parents[1]
SELECTION_PATH = SWE_QA_BENCH_DIR / "zg_bench" / "swe_qa" / "data" / "selection.json"
REFERENCES_PATH = SWE_QA_BENCH_DIR / "zg_bench" / "swe_qa" / "data" / "references.json"
DATASET_PATH = SWE_QA_BENCH_DIR / "datasets"
EXPECTED_TASK_IDS = (
    "reflex:6",
    "sqlfluff:2",
    "conan:1",
    "pylint:10",
    "pylint:9",
    "sympy:38",
    "conan:39",
    "xarray:46",
    "astropy:38",
    "matplotlib:37",
    "streamlink:14",
    "conan:19",
    "django:21",
    "pylint:14",
    "requests:16",
    "django:32",
    "xarray:32",
    "streamlink:43",
    "sympy:26",
    "conan:27",
)

JUDGE_GENERATION_METADATA = {
    "enable_thinking": True,
    "reasoning_effort": "high",
    "max_tokens": 32000,
    "response_format": None,
}


_summary_environment = patch.dict("os.environ", {"GITHUB_STEP_SUMMARY": ""})


def setUpModule() -> None:
    # Direct report-library calls must not append fixture tables to the real
    # Validate job summary. Tests that exercise summary output set a temp path.
    _summary_environment.start()


def tearDownModule() -> None:
    _summary_environment.stop()


def _write_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value), encoding="utf-8")


def _judged_task_report(task_id: str, index: int = 0) -> dict[str, Any]:
    scale = index + 1
    baseline_score = 10 + index % 5
    zvec_score = 12 + index % 5
    category = ("what", "where", "how", "why")[min(index // 5, 3)]
    baseline_total = baseline_score * 5
    zvec_total = zvec_score * 5

    score_keys = (
        "correctness",
        "completeness",
        "relevance",
        "clarity",
        "coherence",
    )

    def profile_result(
        *, profile: str, score: int, metrics: dict[str, int | float]
    ) -> dict[str, Any]:
        scores = {key: score for key in score_keys}
        trials = [
            {
                "trial_index": trial_index,
                "trial_name": (
                    f"{task_id.replace(':', '-')}-{profile}-{trial_index}"
                ),
                "judge": {
                    "label": SELF_JUDGE_LABEL,
                    "model": "glm-5.2",
                    "scores": scores,
                    "total": score * 5,
                    "latency_seconds": 1.0,
                    "usage": {
                        "input_tokens": 50 * scale,
                        "output_tokens": 5 * scale,
                        "cost_usd": 0.01 * scale,
                    },
                },
                "metrics": dict(metrics),
            }
            for trial_index in range(1, 4)
        ]
        return {
            "trial_count": 3,
            "judge": {
                "label": SELF_JUDGE_LABEL,
                "model": "glm-5.2",
                "scores": {key: float(value) for key, value in scores.items()},
                "total": float(score * 5),
                "latency_seconds": 1.0,
                "usage": {
                    "calls": 3,
                    "input_tokens": 150 * scale,
                    "output_tokens": 15 * scale,
                    "cost_usd": 0.03 * scale,
                },
            },
            "metrics": {key: float(value) for key, value in metrics.items()},
            "trials": trials,
        }

    baseline_metrics = {
        "input_tokens": 100 * scale,
        "output_tokens": 20 * scale,
        "tool_calls": 10 * scale,
        "agent_wall_seconds": 20.0 * scale,
        "cost_usd": 0.2 * scale,
    }
    zvec_metrics = {
        "input_tokens": 50 * scale,
        "output_tokens": 15 * scale,
        "tool_calls": 4 * scale,
        "agent_wall_seconds": 10.0 * scale,
        "cost_usd": 0.1 * scale,
    }
    trial_comparison = {
        "judge_delta": zvec_total - baseline_total,
        "input_token_reduction_pct": 50.0,
        "toolcall_reduction_pct": 60.0,
        "time_reduction_pct": 50.0,
        "cost_reduction_pct": 50.0,
    }
    case = {
        "task_id": task_id,
        "role": "smoke" if index == 0 else "category",
        "category": category,
        "trial_count": 3,
        "profiles": {
            "baseline": profile_result(
                profile="baseline",
                score=baseline_score,
                metrics=baseline_metrics,
            ),
            "zvec-grep": profile_result(
                profile="zvec-grep", score=zvec_score, metrics=zvec_metrics
            ),
        },
        "comparison": {
            **trial_comparison,
            "trials": [
                {"trial_index": trial_index, **trial_comparison}
                for trial_index in range(1, 4)
            ],
        },
    }
    return {
        "schema_version": 2,
        "benchmark": "peng-weihan/SWE-QA-Bench",
        "judge": {
            "label": SELF_JUDGE_LABEL,
            "model": "glm-5.2",
            "self_judge": True,
            "temperature": 0,
            "rubric": [
                "correctness",
                "completeness",
                "relevance",
                "clarity",
                "coherence",
            ],
            "usage": {
                "calls": 6,
                "input_tokens": 300 * scale,
                "output_tokens": 30 * scale,
                "cost_usd": 0.06 * scale,
            },
        },
        "gate": {
            "kind": "completion-only",
            "report_only": True,
            "numeric_thresholds": False,
            "expected_tasks": [task_id],
            "valid_pairs": 1,
            "successful_judgements": 6,
            "passed": True,
        },
        "cases": [case],
        "aggregate": _aggregate([case]),
    }


def _set_report_trial_metrics(
    report: dict[str, Any],
    *,
    baseline: list[tuple[int, int, float, float | None]],
    zvec: list[tuple[int, int, float, float | None]],
) -> None:
    case = report["cases"][0]
    for profile_name, rows in (("baseline", baseline), ("zvec-grep", zvec)):
        profile = case["profiles"][profile_name]
        for trial, (input_tokens, tool_calls, wall_seconds, cost_usd) in zip(
            profile["trials"], rows, strict=True
        ):
            trial["metrics"].update(
                {
                    "input_tokens": input_tokens,
                    "tool_calls": tool_calls,
                    "agent_wall_seconds": wall_seconds,
                    "cost_usd": cost_usd,
                }
            )
        profile["metrics"].update(
            {
                "input_tokens": sum(row[0] for row in rows) / len(rows),
                "tool_calls": sum(row[1] for row in rows) / len(rows),
                "agent_wall_seconds": sum(row[2] for row in rows) / len(rows),
                "cost_usd": (
                    None
                    if any(row[3] is None for row in rows)
                    else sum(float(row[3]) for row in rows if row[3] is not None)
                    / len(rows)
                ),
            }
        )

    baseline_summary = case["profiles"]["baseline"]
    zvec_summary = case["profiles"]["zvec-grep"]

    def reduction(key: str) -> float | None:
        baseline_value = baseline_summary["metrics"][key]
        zvec_value = zvec_summary["metrics"][key]
        if baseline_value is None or zvec_value is None or baseline_value == 0:
            return None
        return (baseline_value - zvec_value) / baseline_value * 100

    # Keep the serialized task summary aligned with its displayed profile means.
    # Individual tests may overwrite it to simulate a stale source artifact.
    case["comparison"] = {
        "judge_delta": (
            zvec_summary["judge"]["total"]
            - baseline_summary["judge"]["total"]
        ),
        "input_token_reduction_pct": reduction("input_tokens"),
        "toolcall_reduction_pct": reduction("tool_calls"),
        "time_reduction_pct": reduction("agent_wall_seconds"),
        "cost_reduction_pct": reduction("cost_usd"),
    }


def _set_report_trial_judgements(
    report: dict[str, Any], *, baseline: list[int], zvec: list[int]
) -> None:
    """Set complete, consistent Judge evidence with an arbitrary trial count."""
    assert len(baseline) == len(zvec)
    case = report["cases"][0]
    count = len(baseline)
    case["trial_count"] = count
    for profile_name, totals in (("baseline", baseline), ("zvec-grep", zvec)):
        profile = case["profiles"][profile_name]
        source_trials = profile["trials"]
        trials = []
        for index, total in enumerate(totals, start=1):
            trial = copy.deepcopy(source_trials[(index - 1) % len(source_trials)])
            trial["trial_index"] = index
            trial["trial_name"] = f"{case['task_id']}-{profile_name}-{index}"
            quotient, remainder = divmod(total, 5)
            trial["judge"]["scores"] = {
                key: quotient + (position < remainder)
                for position, key in enumerate(trial["judge"]["scores"])
            }
            trial["judge"]["total"] = total
            trials.append(trial)
        profile["trials"] = trials
        profile["trial_count"] = count
        profile["judge"]["total"] = sum(totals) / count
        profile["judge"]["scores"] = {
            key: sum(trial["judge"]["scores"][key] for trial in trials) / count
            for key in profile["judge"]["scores"]
        }
        profile["judge"]["usage"] = {
            "calls": count,
            **{
                key: sum(trial["judge"]["usage"][key] for trial in trials)
                for key in ("input_tokens", "output_tokens", "cost_usd")
            },
        }
    report["gate"]["successful_judgements"] = count * 2
    report["judge"]["usage"] = {
        key: sum(profile["judge"]["usage"][key] for profile in case["profiles"].values())
        for key in ("calls", "input_tokens", "output_tokens", "cost_usd")
    }
    comparison = case["comparison"]
    comparison["judge_delta"] = sum(zvec) / count - sum(baseline) / count
    comparison["trials"] = [
        {
            "trial_index": index,
            **{key: value for key, value in comparison.items() if key != "trials"},
            "judge_delta": zvec_score - baseline_score,
        }
        for index, (baseline_score, zvec_score) in enumerate(zip(baseline, zvec, strict=True), start=1)
    ]
    report["aggregate"] = _aggregate([case])


def _write_harbor_job(
    root: Path,
    *,
    profile: str,
    trials: list[dict[str, Any]],
) -> None:
    job_dir = root / f"fixture-reflex-6-{profile}"
    _write_json(
        job_dir / "result.json",
        {
            "finished_at": "2026-08-11T10:01:00+00:00",
            "n_total_trials": len(trials),
            "stats": {
                "n_completed_trials": len(trials),
                "n_errored_trials": 0,
            },
        },
    )
    for trial_position, trial in enumerate(trials, start=1):
        trial_dir = job_dir / str(trial["trial_name"])
        wall_seconds = int(trial["agent_wall_seconds"])
        started_second = trial_position
        finished_second = started_second + wall_seconds
        _write_json(
            trial_dir / "result.json",
            {
                "task_name": "reflex-6",
                "task_id": {"path": "/dataset/reflex-6"},
                "trial_name": trial_dir.name,
                "finished_at": f"2026-08-11T10:00:{wall_seconds:02d}+00:00",
                "exception_info": None,
                "agent_info": {
                    "name": "opencode",
                    "model_info": {"name": "custom-openai/glm-5.2"},
                },
                "agent_result": {
                    "n_input_tokens": trial["input_tokens"],
                    "n_output_tokens": trial["output_tokens"],
                    "cost_usd": trial["cost_usd"],
                },
                "verifier_result": {"rewards": {"reward": 1}},
                "agent_execution": {
                    "started_at": (
                        f"2026-08-11T10:00:{started_second:02d}+00:00"
                    ),
                    "finished_at": (
                        f"2026-08-11T10:00:{finished_second:02d}+00:00"
                    ),
                },
            },
        )
        calls = [
            {
                "tool_call_id": f"call-{index}",
                "function_name": "bash",
                "arguments": {"command": "true"},
            }
            for index in range(trial["tool_calls"])
        ]
        _write_json(
            trial_dir / "agent" / "trajectory.json",
            {
                "schema_version": "ATIF-v1.7",
                "agent": {
                    "name": "opencode",
                    "version": "1.18.4",
                    "model_name": "custom-openai/glm-5.2",
                },
                "steps": [
                    {"step_id": 1, "source": "user", "message": "question"},
                    {
                        "step_id": 2,
                        "source": "agent",
                        "message": trial["answer"],
                        "tool_calls": calls,
                    },
                ],
            },
        )


def _harbor_trial(
    trial_name: str,
    *,
    answer: str,
    input_tokens: int,
    output_tokens: int,
    tool_calls: int,
    agent_wall_seconds: int,
    cost_usd: float | None,
) -> dict[str, Any]:
    return {
        "trial_name": trial_name,
        "answer": answer,
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "tool_calls": tool_calls,
        "agent_wall_seconds": agent_wall_seconds,
        "cost_usd": cost_usd,
    }


class CollectTests(unittest.TestCase):
    def test_collects_three_sorted_trials_from_each_harbor_job(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            _write_harbor_job(
                root,
                profile="baseline",
                trials=[
                    _harbor_trial(
                        "reflex-6-baseline-c",
                        answer="baseline c",
                        input_tokens=300,
                        output_tokens=30,
                        tool_calls=9,
                        agent_wall_seconds=30,
                        cost_usd=0.09,
                    ),
                    _harbor_trial(
                        "reflex-6-baseline-a",
                        answer="baseline a",
                        input_tokens=100,
                        output_tokens=10,
                        tool_calls=3,
                        agent_wall_seconds=10,
                        cost_usd=0.03,
                    ),
                    _harbor_trial(
                        "reflex-6-baseline-b",
                        answer="baseline b",
                        input_tokens=200,
                        output_tokens=20,
                        tool_calls=6,
                        agent_wall_seconds=20,
                        cost_usd=0.06,
                    ),
                ],
            )
            _write_harbor_job(
                root,
                profile="zvec-grep",
                trials=[
                    _harbor_trial(
                        f"reflex-6-zvec-{suffix}",
                        answer=f"zvec {suffix}",
                        input_tokens=70 + index,
                        output_tokens=15 + index,
                        tool_calls=1 + index,
                        agent_wall_seconds=10 + index,
                        cost_usd=None,
                    )
                    for index, suffix in enumerate(("c", "a", "b"))
                ],
            )
            output = root / "pairs" / "reflex-6" / "pair.json"

            pair = collect_pair(
                runs_dir=root,
                task="reflex:6",
                output=output,
                expected_trials=3,
            )

            self.assertTrue(pair["valid"])
            self.assertEqual(pair["schema_version"], 2)
            self.assertEqual(pair["task_id"], "reflex:6")
            self.assertEqual(pair["expected_trials"], 3)
            self.assertEqual(pair["actual_trials"], 3)
            baseline = pair["profiles"]["baseline"]
            self.assertEqual(baseline["trial_count"], 3)
            self.assertEqual(
                [trial["trial_name"] for trial in baseline["trials"]],
                [
                    "reflex-6-baseline-c",
                    "reflex-6-baseline-a",
                    "reflex-6-baseline-b",
                ],
            )
            self.assertEqual(
                [trial["trial_index"] for trial in baseline["trials"]],
                [1, 2, 3],
            )
            self.assertEqual(baseline["trials"][1]["input_tokens"], 100)
            self.assertEqual(baseline["trials"][1]["tool_calls"], 3)
            self.assertEqual(
                baseline["trials"][1]["agent_wall_seconds"], 10.0
            )
            self.assertTrue(
                all(
                    trial["cost_usd"] is None
                    for trial in pair["profiles"]["zvec-grep"]["trials"]
                )
            )
            self.assertEqual(json.loads(output.read_text()), pair)

    def test_retry_history_does_not_add_trials_or_hide_final_errors(self) -> None:
        for exhausted in (False, True):
            with (
                self.subTest(exhausted=exhausted),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                for profile in ("baseline", "zvec-grep"):
                    _write_harbor_job(
                        root,
                        profile=profile,
                        trials=[
                            _harbor_trial(
                                f"reflex-6-{profile}-{index}",
                                answer="terminal answer",
                                input_tokens=100,
                                output_tokens=10,
                                tool_calls=2,
                                agent_wall_seconds=5,
                                cost_usd=None,
                            )
                            for index in range(1, 6)
                        ],
                    )
                    job_dir = root / f"fixture-reflex-6-{profile}"
                    trial_dir = job_dir / f"reflex-6-{profile}-1"
                    failure = json.loads((trial_dir / "result.json").read_text())
                    failure["exception_info"] = {
                        "exception_type": "AgentTimeoutError"
                    }
                    failure["agent_result"]["n_input_tokens"] = 1000
                    for attempt in (1, 2):
                        archive = (
                            job_dir
                            / ".retry-history"
                            / trial_dir.name
                            / f"attempt-{attempt}"
                        )
                        shutil.copytree(trial_dir, archive)
                        _write_json(archive / "result.json", failure)

                    has_final_error = exhausted and profile == "baseline"
                    if has_final_error:
                        _write_json(trial_dir / "result.json", failure)
                    job_result = json.loads((job_dir / "result.json").read_text())
                    job_result["stats"].update(
                        n_retries=2, n_errored_trials=int(has_final_error)
                    )
                    _write_json(job_dir / "result.json", job_result)

                output = root / "pair.json"
                if exhausted:
                    with self.assertRaisesRegex(SweQaError, "errored trials"):
                        collect_pair(
                            runs_dir=root,
                            task="reflex:6",
                            output=output,
                            expected_trials=5,
                        )
                    self.assertFalse(output.exists())
                else:
                    pair = collect_pair(
                        runs_dir=root,
                        task="reflex:6",
                        output=output,
                        expected_trials=5,
                    )
                    for profile in pair["profiles"].values():
                        self.assertEqual(profile["trial_count"], 5)
                        self.assertEqual(
                            [trial["input_tokens"] for trial in profile["trials"]],
                            [100] * 5,
                        )

    def test_empty_final_answer_fails_collection(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            for profile in ("baseline", "zvec-grep"):
                _write_harbor_job(
                    root,
                    profile=profile,
                    trials=[
                        _harbor_trial(
                            f"reflex-6-{profile}-{index}",
                            answer=(
                                ""
                                if profile == "zvec-grep" and index == 2
                                else "answer"
                            ),
                            input_tokens=10,
                            output_tokens=2,
                            tool_calls=0,
                            agent_wall_seconds=10,
                            cost_usd=None,
                        )
                        for index in range(1, 4)
                    ],
                )

            with self.assertRaisesRegex(SweQaError, "empty final answer"):
                collect_pair(
                    runs_dir=root,
                    task="reflex:6",
                    output=root / "pair.json",
                    expected_trials=3,
                )

    def test_collect_rejects_wrong_trial_count(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            for profile in ("baseline", "zvec-grep"):
                _write_harbor_job(
                    root,
                    profile=profile,
                    trials=[
                        _harbor_trial(
                            f"reflex-6-{profile}-{index}",
                            answer="answer",
                            input_tokens=10,
                            output_tokens=2,
                            tool_calls=0,
                            agent_wall_seconds=10,
                            cost_usd=None,
                        )
                        for index in range(1, 3)
                    ],
                )

            with self.assertRaisesRegex(SweQaError, "expected exactly 3"):
                collect_pair(
                    runs_dir=root,
                    task="reflex:6",
                    output=root / "pair.json",
                    expected_trials=3,
                )


def _session_usage_fixture() -> dict[str, Any]:
    def metric(
        uncached: int, cache_read: int, cache_write: int,
        text: int, reasoning: int, tools: int, calls: int, cost: float,
    ) -> dict[str, int | float]:
        return {
            "input_tokens": uncached + cache_read + cache_write,
            "output_tokens": text + reasoning,
            "uncached_input_tokens": uncached,
            "cache_read_tokens": cache_read,
            "cache_write_tokens": cache_write,
            "text_output_tokens": text,
            "reasoning_tokens": reasoning,
            "tool_calls": tools, "llm_calls": calls, "cost_usd": cost,
        }

    root = metric(75, 20, 5, 16, 4, 1, 2, 0.1)
    child = metric(100, 100, 0, 25, 15, 3, 2, 0.2)
    grandchild = metric(90, 200, 10, 40, 20, 4, 3, 0.3)
    descendants = {key: child[key] + grandchild[key] for key in SESSION_USAGE_METRICS}
    total = {key: root[key] + descendants[key] for key in SESSION_USAGE_METRICS}
    return {
        "schema_version": 1, "scope": SESSION_USAGE_SCOPE,
        "complete": True, "errors": [], "root_session_id": "session-root",
        "root": root, "descendants": descendants, "total": total,
        "collection_wall_seconds": 2.0,
        "sessions": [
            {"session_id": "session-root", "parent_session_id": None, **root},
            {"session_id": "session-child", "parent_session_id": "session-root", **child},
            {"session_id": "session-grandchild", "parent_session_id": "session-child", **grandchild},
        ],
    }


class SessionUsageReportingTests(unittest.TestCase):
    def _jobs(self, root: Path) -> list[Path]:
        trial_dirs = []
        for profile in ("baseline", "zvec-grep"):
            usage = _session_usage_fixture()
            _write_harbor_job(root, profile=profile, trials=[_harbor_trial(
                f"reflex-6-{profile}", answer=f"root final {profile}",
                input_tokens=600, output_tokens=120, tool_calls=1,
                agent_wall_seconds=22, cost_usd=usage["total"]["cost_usd"],
            )])
            trial_dir = root / f"fixture-reflex-6-{profile}" / f"reflex-6-{profile}"
            result = json.loads((trial_dir / "result.json").read_text())
            result["config"] = {"agent": {"kwargs": {"collect_session_usage": True}}}
            result["agent_result"]["n_cache_tokens"] = 335
            _write_json(trial_dir / "result.json", result)
            _write_json(trial_dir / "agent" / "session-usage.json", usage)
            trial_dirs.append(trial_dir)
        return trial_dirs

    def test_collects_nested_usage_and_preserves_root_answer_and_wall_time(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            self._jobs(root)
            pair = collect_pair(runs_dir=root, task="reflex:6", output=root / "pair.json")
            self.assertEqual(pair["usage_scope"], SESSION_USAGE_SCOPE)
            for name, profile in pair["profiles"].items():
                trial = profile["trials"][0]
                self.assertEqual(trial["answer"], f"root final {name}")
                self.assertEqual(trial["input_tokens"], 600)
                self.assertEqual(trial["output_tokens"], 120)
                self.assertEqual(trial["reasoning_tokens"], 39)
                self.assertEqual(trial["cache_read_tokens"], 320)
                self.assertEqual(trial["cache_write_tokens"], 15)
                self.assertEqual(trial["tool_calls"], 8)
                self.assertEqual(trial["session_usage"]["root"]["tool_calls"], 1)
                self.assertEqual(trial["session_usage"]["descendants"]["tool_calls"], 7)
                # Child work already overlaps the measured root wall interval.
                self.assertEqual(trial["agent_wall_seconds"], 20)
                self.assertEqual(trial["usage_collection_wall_seconds"], 2)

    def test_required_usage_missing_or_incomplete_fails_closed(self) -> None:
        for failure in ("missing", "incomplete", "errors"):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                trial_dir = self._jobs(root)[0]
                path = trial_dir / "agent" / "session-usage.json"
                if failure == "missing":
                    path.unlink()
                else:
                    usage = json.loads(path.read_text())
                    if failure == "incomplete":
                        usage["complete"] = False
                    else:
                        usage["errors"] = ["missing child session"]
                    _write_json(path, usage)
                with self.assertRaisesRegex(SweQaError, "session usage"):
                    collect_pair(runs_dir=root, task="reflex:6", output=root / "pair.json")
                self.assertFalse((root / "pair.json").exists())

    def test_rejects_inconsistent_totals_context_and_collection_time(self) -> None:
        for failure in ("total", "input_components", "output_components", "context", "cache_context", "wall"):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                trial_dir = self._jobs(root)[0]
                path = trial_dir / "agent" / "session-usage.json"
                usage = json.loads(path.read_text())
                result = json.loads((trial_dir / "result.json").read_text())
                if failure == "total":
                    usage["total"]["tool_calls"] += 1
                elif failure == "input_components":
                    usage["root"]["uncached_input_tokens"] += 1
                elif failure == "output_components":
                    usage["root"]["reasoning_tokens"] += 1
                elif failure == "context":
                    result["agent_result"]["n_input_tokens"] = 100
                elif failure == "cache_context":
                    result["agent_result"]["n_cache_tokens"] = 320
                else:
                    usage["collection_wall_seconds"] = 30
                _write_json(path, usage)
                _write_json(trial_dir / "result.json", result)
                with self.assertRaises(SweQaError):
                    collect_pair(runs_dir=root, task="reflex:6", output=root / "pair.json")

    def test_unknown_cost_propagates_with_or_without_children(self) -> None:
        for children in (True, False):
            with self.subTest(children=children), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                for trial_dir in self._jobs(root):
                    path = trial_dir / "agent" / "session-usage.json"
                    usage = json.loads(path.read_text())
                    usage["root"]["cost_usd"] = None
                    if children:
                        usage["descendants"]["cost_usd"] = None
                        usage["total"]["cost_usd"] = None
                    else:
                        usage["descendants"] = {key: 0 for key in SESSION_USAGE_METRICS}
                        usage["total"] = dict(usage["root"])
                        usage["sessions"] = usage["sessions"][:1]
                    result = json.loads((trial_dir / "result.json").read_text())
                    total = usage["total"]
                    result["agent_result"].update(
                        n_input_tokens=total["input_tokens"], n_output_tokens=total["output_tokens"],
                        n_cache_tokens=total["cache_read_tokens"] + total["cache_write_tokens"],
                        cost_usd=None,
                    )
                    _write_json(path, usage)
                    _write_json(trial_dir / "result.json", result)
                pair = collect_pair(runs_dir=root, task="reflex:6", output=root / "pair.json")
                self.assertIsNone(pair["profiles"]["baseline"]["trials"][0]["cost_usd"])

    def test_collection_rejects_mixed_legacy_and_session_tree_profiles(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            trial_dir = self._jobs(root)[0]
            result = json.loads((trial_dir / "result.json").read_text())
            result.pop("config")
            _write_json(trial_dir / "result.json", result)
            (trial_dir / "agent" / "session-usage.json").unlink()
            with self.assertRaisesRegex(SweQaError, "usage scopes"):
                collect_pair(runs_dir=root, task="reflex:6", output=root / "pair.json")

    def test_judge_uses_root_answer_and_reports_full_session_metrics(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            self._jobs(root)
            pair_path = root / "pair.json"
            collect_pair(runs_dir=root, task="reflex:6", output=pair_path)
            references = root / "references.json"
            _write_json(references, {"references": [{
                "task_id": "reflex:6", "question": "question", "reference_answer": "reference",
            }]})
            prompts: list[str] = []

            def completion(**kwargs: Any) -> dict[str, Any]:
                prompts.append(kwargs["messages"][0]["content"])
                return {
                    "choices": [{"message": {"content": json.dumps({key: 18 for key in (
                        "correctness", "completeness", "relevance", "clarity", "coherence"
                    )})}}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 10},
                }

            with patch.dict("os.environ", {"GLM_API_KEY": "mock"}):
                report = judge_pairs(
                    pairs_root=pair_path, references_path=references,
                    output_dir=root / "report", expected=["reflex:6"], completion_fn=completion,
                )
            self.assertEqual(report["usage_scope"], SESSION_USAGE_SCOPE)
            self.assertEqual(len(prompts), 2)
            self.assertTrue(all("Candidate answer:\nroot final " in prompt for prompt in prompts))
            for profile in report["aggregate"]["profiles"].values():
                self.assertEqual(profile["input_tokens"], 600)
                self.assertEqual(profile["tool_calls"], 8)
                self.assertEqual(profile["agent_wall_seconds"], 20)
                self.assertEqual(profile["session_usage"]["descendants"]["input_tokens"], 500)
            markdown = (root / "report" / "report.md").read_text()
            self.assertNotIn("Text output / reasoning", markdown)
            self.assertNotIn("Cache read / write", markdown)
            self.assertNotIn("| descendants |", markdown)
            self.assertNotIn("Root/subagent", markdown)
            self.assertIn("not a complete provider bill", markdown)
            aggregated = aggregate_reports(reports_root=root / "report", output_dir=root / "aggregate")
            self.assertEqual(aggregated["usage_scope"], SESSION_USAGE_SCOPE)

    def _new_report(self, task_id: str) -> dict[str, Any]:
        report = _judged_task_report(task_id)
        report["usage_scope"] = SESSION_USAGE_SCOPE
        usage = _session_usage_fixture()
        for profile in report["cases"][0]["profiles"].values():
            for metrics in [profile["metrics"], *[trial["metrics"] for trial in profile["trials"]]]:
                metrics.update(copy.deepcopy(usage["total"]))
                metrics["usage_scope"] = SESSION_USAGE_SCOPE
                metrics["session_usage"] = copy.deepcopy(usage)
        return report

    def test_aggregate_rejects_legacy_without_scope_mixed_with_new_reports(self) -> None:
        for new_first in (True, False):
            with self.subTest(new_first=new_first), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                new = self._new_report("reflex:6")
                legacy = _judged_task_report("requests:16")
                self.assertNotIn("usage_scope", legacy)
                for index, report in enumerate((new, legacy) if new_first else (legacy, new)):
                    _write_json(root / str(index) / "report.json", report)
                with self.assertRaisesRegex(SweQaError, "usage scopes"):
                    aggregate_reports(reports_root=root, output_dir=root / "aggregate")

    def test_report_rejects_inconsistent_summary_or_mixed_trial_scope(self) -> None:
        for failure in ("mean", "trial_scope"):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                report = self._new_report("reflex:6")
                profile = report["cases"][0]["profiles"]["baseline"]
                if failure == "mean":
                    profile["metrics"]["agent_wall_seconds"] += 1
                else:
                    metrics = profile["trials"][0]["metrics"]
                    metrics.pop("usage_scope")
                    metrics.pop("session_usage")
                _write_json(root / "task" / "report.json", report)
                with self.assertRaises(SweQaError):
                    aggregate_reports(reports_root=root, output_dir=root / "aggregate")


class JudgeTests(unittest.TestCase):
    def test_efficiency_change_displays_savings_as_negative(self) -> None:
        self.assertEqual(
            _metric_cell(100, 50, 50),
            "100.00 / 50.00 / -50.00%",
        )
        self.assertEqual(
            _metric_cell(100, 125, -25),
            "100.00 / 125.00 / +25.00%",
        )
        self.assertEqual(
            _metric_cell(100, 100, 0),
            "100.00 / 100.00 / +0.00%",
        )
        self.assertEqual(_metric_cell(0, 1, None), "0.00 / 1.00 / N/A")

    def _write_pair_and_reference(self, root: Path) -> tuple[Path, Path]:
        pairs_root = root / "pairs"
        baseline_metrics = [
            (100, 20, 10, 10.0, 1.0),
            (900, 30, 90, 90.0, 9.0),
            (100, 10, 10, 20.0, 2.0),
        ]
        zvec_metrics = [
            (10, 10, 1, 1.0, 0.1),
            (900, 20, 90, 90.0, 9.0),
            (50, 8, 5, 10.0, 1.0),
        ]

        def trials(
            profile: str, rows: list[tuple[int, int, int, float, float]]
        ) -> list[dict[str, Any]]:
            return [
                {
                    "trial_index": index,
                    "trial_name": f"reflex-6-{profile}-{index}",
                    "answer": f"{profile} candidate {index}",
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "tool_calls": tool_calls,
                    "agent_wall_seconds": wall_seconds,
                    "cost_usd": cost_usd,
                }
                for index, (
                    input_tokens,
                    output_tokens,
                    tool_calls,
                    wall_seconds,
                    cost_usd,
                ) in enumerate(rows, start=1)
            ]

        _write_json(
            pairs_root / "pair-reflex-6.json",
            {
                "schema_version": 2,
                "task_id": "reflex-6",
                "valid": True,
                "expected_trials": 3,
                "actual_trials": 3,
                "profiles": {
                    "baseline": {
                        "profile": "baseline",
                        "trial_count": 3,
                        "trials": trials("baseline", baseline_metrics),
                    },
                    "zvec-grep": {
                        "profile": "zvec-grep",
                        "trial_count": 3,
                        "trials": trials("zvec-grep", zvec_metrics),
                    },
                },
            },
        )
        references = root / "references.json"
        _write_json(
            references,
            {
                "references": [
                    {
                        "task_id": "reflex:6",
                        "question": "the question",
                        "reference_answer": "judge-only reference",
                        "role": "smoke",
                        "category": "smoke",
                    }
                ]
            },
        )
        return pairs_root, references

    def test_judges_each_candidate_and_writes_report_and_summary(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            pairs_root, references = self._write_pair_and_reference(root)
            output_dir = root / "report"
            summary = root / "step-summary.md"
            requests: list[dict[str, Any]] = []

            def fake_completion(**kwargs: Any) -> dict[str, Any]:
                requests.append(kwargs)
                prompt_text = kwargs["messages"][0]["content"]
                candidate_scores = {
                    "baseline candidate 1": 10,
                    "baseline candidate 2": 12,
                    "baseline candidate 3": 14,
                    "zvec-grep candidate 1": 12,
                    "zvec-grep candidate 2": 14,
                    "zvec-grep candidate 3": 16,
                }
                score = next(
                    value
                    for candidate, value in candidate_scores.items()
                    if f"Candidate answer:\n{candidate}" in prompt_text
                )
                content = json.dumps(
                    {
                        "correctness": score,
                        "completeness": score,
                        "relevance": score,
                        "clarity": score,
                        "coherence": score,
                    }
                )
                return {
                    "choices": [{"message": {"content": content}}],
                    "usage": {"prompt_tokens": 50, "completion_tokens": 5},
                    "_hidden_params": {"response_cost": 0.01},
                }

            with patch.dict(
                "os.environ",
                {
                    "GLM_API_KEY": "test-secret",
                    "GLM_BASE_URL": "https://example.invalid/v1",
                    "GITHUB_STEP_SUMMARY": str(summary),
                },
                clear=True,
            ):
                report = judge_pairs(
                    pairs_root=pairs_root,
                    references_path=references,
                    output_dir=output_dir,
                    expected=["reflex-6"],
                    completion_fn=fake_completion,
                    attempts=1,
                )

            self.assertEqual(len(requests), 6)
            self.assertTrue(all(call["temperature"] == 0 for call in requests))
            self.assertTrue(all(call["seed"] == 42 for call in requests))
            for call in requests:
                self.assertEqual(call["extra_body"], {"enable_thinking": True})
                self.assertEqual(call["reasoning_effort"], "high")
                self.assertEqual(call["max_tokens"], 32000)
                self.assertNotIn("response_format", call)
            self.assertTrue(
                all(call["model"] == "openai/glm-5.2" for call in requests)
            )
            self.assertTrue(
                all(call["api_key"] == "test-secret" for call in requests)
            )
            self.assertEqual(report["schema_version"], 2)
            self.assertEqual(report["judge"]["label"], SELF_JUDGE_LABEL)
            self.assertTrue(report["judge"]["self_judge"])
            self.assertEqual(report["judge"]["temperature"], 0)
            self.assertEqual(report["judge"]["seed"], 42)
            for key, value in JUDGE_GENERATION_METADATA.items():
                self.assertEqual(report["judge"][key], value)
            self.assertEqual(report["judge"]["usage"]["calls"], 6)
            self.assertEqual(report["judge"]["usage"]["input_tokens"], 300)
            self.assertEqual(report["judge"]["usage"]["output_tokens"], 30)
            self.assertAlmostEqual(report["judge"]["usage"]["cost_usd"], 0.06)
            self.assertTrue(report["gate"]["report_only"])
            self.assertFalse(report["gate"]["numeric_thresholds"])
            self.assertEqual(report["gate"]["successful_judgements"], 6)

            case = report["cases"][0]
            self.assertEqual(case["task_id"], "reflex:6")
            self.assertEqual(case["trial_count"], 3)
            baseline = case["profiles"]["baseline"]
            zvec = case["profiles"]["zvec-grep"]
            self.assertEqual(baseline["trial_count"], 3)
            self.assertEqual(zvec["trial_count"], 3)
            self.assertEqual(baseline["judge"]["scores"]["correctness"], 12.0)
            self.assertEqual(baseline["judge"]["total"], 60.0)
            self.assertEqual(zvec["judge"]["scores"]["correctness"], 14.0)
            self.assertEqual(zvec["judge"]["total"], 70.0)
            self.assertEqual(
                [trial["judge"]["total"] for trial in baseline["trials"]],
                [50, 60, 70],
            )
            self.assertEqual(
                [trial["judge"]["total"] for trial in zvec["trials"]],
                [60, 70, 80],
            )
            self.assertAlmostEqual(
                baseline["metrics"]["input_tokens"], 1100 / 3
            )
            self.assertEqual(zvec["metrics"]["input_tokens"], 320.0)

            comparison = case["comparison"]
            self.assertEqual(comparison["judge_delta"], 10.0)
            self.assertEqual(
                [trial["trial_index"] for trial in comparison["trials"]],
                [1, 2, 3],
            )
            profile_mean_ratio = (1100 / 3 - 320) / (1100 / 3) * 100
            self.assertAlmostEqual(
                comparison["input_token_reduction_pct"], profile_mean_ratio
            )
            self.assertAlmostEqual(
                comparison["toolcall_reduction_pct"], profile_mean_ratio
            )
            self.assertAlmostEqual(
                comparison["time_reduction_pct"], (120 - 101) / 120 * 100
            )
            self.assertEqual(
                [
                    trial["input_token_reduction_pct"]
                    for trial in comparison["trials"]
                ],
                [90.0, 0.0, 50.0],
            )
            markdown = (output_dir / "report.md").read_text()
            self.assertIn("Aggregate", markdown)
            self.assertIn("input_token", markdown)
            self.assertIn("60.00 / 70.00 / +10.00", markdown)
            self.assertIn("366.67 / 320.00 / -12.73%", markdown)
            self.assertIn("calculated directly from the displayed Aggregate values", markdown)
            self.assertIn("not an average of task percentages", markdown)
            self.assertLess(markdown.index("| **Aggregate** |"), markdown.index("| reflex:6 |"))
            self.assertNotIn("cost", markdown.lower())
            self.assertNotIn("$", markdown)
            self.assertEqual(summary.read_text(), markdown)
            serialized = (output_dir / "report.json").read_text()
            self.assertNotIn("judge-only reference", serialized)
            self.assertNotIn("test-secret", serialized)

    def test_judge_retries_preserve_generation_parameters_and_prompt(self) -> None:
        requests: list[dict[str, Any]] = []

        def completion(**kwargs: Any) -> dict[str, Any]:
            requests.append(copy.deepcopy(kwargs))
            if len(requests) == 1:
                raise ConnectionError("temporary transport failure")
            content = (
                "invalid JSON" if len(requests) == 2 else json.dumps(
                    {key: 10 for key in (
                        "correctness", "completeness", "relevance",
                        "clarity", "coherence",
                    )}
                )
            )
            return {"choices": [{"message": {"content": content}}]}

        with patch("zg_bench.swe_qa.judge.time.sleep"):
            result = _judge_candidate(
                completion_fn=completion,
                api_key="test-secret",
                api_base="https://example.invalid/v1",
                question="question",
                reference="reference",
                candidate="candidate",
                attempts=3,
            )
        self.assertEqual(result["total"], 50)
        self.assertEqual(len(requests), 3)
        self.assertEqual(requests[0], requests[1])
        self.assertEqual(requests[1], requests[2])
        self.assertEqual(requests[0]["temperature"], 0)
        self.assertEqual(requests[0]["seed"], 42)
        self.assertEqual(requests[0]["extra_body"], {"enable_thinking": True})
        self.assertEqual(requests[0]["reasoning_effort"], "high")
        self.assertEqual(requests[0]["max_tokens"], 32000)
        self.assertNotIn("response_format", requests[0])

    def test_litellm_forwards_judge_generation_parameters_to_http(self) -> None:
        requests: list[dict[str, Any]] = []
        content = json.dumps({key: 10 for key in (
            "correctness", "completeness", "relevance", "clarity", "coherence"
        )})

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args: Any) -> None:
                pass

            def do_POST(self) -> None:
                length = int(self.headers["Content-Length"])
                requests.append(json.loads(self.rfile.read(length)))
                body = json.dumps({
                    "id": "local-judge-response",
                    "object": "chat.completion",
                    "created": 1,
                    "model": requests[-1]["model"],
                    "choices": [{
                        "index": 0,
                        "finish_reason": "stop",
                        "message": {"role": "assistant", "content": content},
                    }],
                    "usage": {
                        "prompt_tokens": 20,
                        "completion_tokens": 10,
                        "total_tokens": 30,
                    },
                }).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with patch.dict("os.environ", {
                "LITELLM_LOCAL_MODEL_COST_MAP": "True", "DO_NOT_TRACK": "True"
            }):
                for model in ("glm-5.2", "qwen3.8-max"):
                    with self.subTest(model=model):
                        result = _judge_candidate(
                            completion_fn=_default_completion(),
                            api_key="local-test-key",
                            api_base=f"http://127.0.0.1:{server.server_port}/v1",
                            question="question",
                            reference="reference",
                            candidate="candidate",
                            attempts=1,
                            model=model,
                        )
                        self.assertEqual(result["total"], 50)
                        self.assertEqual(result["model"], model)
                        self.assertEqual(result["label"], f"{model}-self-judge-v1")
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)

        self.assertEqual(len(requests), 2)
        for request, model in zip(requests, ("glm-5.2", "qwen3.8-max"), strict=True):
            with self.subTest(model=model):
                self.assertEqual(request["model"], model)
                self.assertEqual(request["temperature"], OPENCODE_QWEN_TEMPERATURE if model == "qwen3.8-max" else 0)
                self.assertEqual(request["seed"], 42)
                self.assertIs(request["enable_thinking"], OPENCODE_QWEN_ENABLE_THINKING if model == "qwen3.8-max" else True)
                self.assertEqual(request["reasoning_effort"], OPENCODE_QWEN_REASONING_EFFORT if model == "qwen3.8-max" else "high")
                self.assertEqual(request["max_tokens"], 32000)
                self.assertNotIn("response_format", request)
                self.assertNotIn("extra_body", request)
                self.assertNotIn("reasoningEffort", request)

    def test_qwen_cli_propagates_model_and_roundtrips_report_identity(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            pairs_root, references = self._write_pair_and_reference(root)
            requests: list[dict[str, Any]] = []

            def completion(**kwargs: Any) -> dict[str, Any]:
                requests.append(kwargs)
                return {
                    "choices": [{"message": {"content": json.dumps({key: 10 for key in (
                        "correctness", "completeness", "relevance", "clarity", "coherence"
                    )})}}],
                    "usage": {"prompt_tokens": 20, "completion_tokens": 10},
                }

            with (
                patch.dict("os.environ", {
                    "OPENAI_API_KEY": "shared-key",
                    "OPENAI_BASE_URL": "https://shared.invalid/v1",
                    "GLM_API_KEY": "unused-legacy-key",
                    "GLM_BASE_URL": "https://legacy.invalid/v1",
                }, clear=True),
                patch("zg_bench.swe_qa.judge._default_completion", return_value=completion),
                patch("builtins.print"),
            ):
                status = swe_qa_main([
                    "judge", "--pairs-root", str(pairs_root),
                    "--references", str(references),
                    "--output-dir", str(root / "report"),
                    "--expected", "reflex-6", "--model", "qwen3.8-max", "--attempts", "1",
                ])
            self.assertEqual(status, 0)
            self.assertEqual(len(requests), 6)
            for request in requests:
                self.assertEqual(request["model"], "openai/qwen3.8-max")
                self.assertEqual(request["api_key"], "unused-legacy-key")
                self.assertEqual(request["api_base"], "https://legacy.invalid/v1")
                self.assertEqual(request["temperature"], OPENCODE_QWEN_TEMPERATURE)
                self.assertEqual(request["seed"], 42)
                self.assertEqual(request["extra_body"], {"enable_thinking": OPENCODE_QWEN_ENABLE_THINKING})
                self.assertEqual(request["reasoning_effort"], OPENCODE_QWEN_REASONING_EFFORT)
                self.assertNotIn("response_format", request)
            report = aggregate_reports(reports_root=root / "report", output_dir=root / "combined")
            self.assertEqual(report["judge"]["model"], "qwen3.8-max")
            self.assertEqual(report["judge"]["label"], "qwen3.8-max-self-judge-v1")
            self.assertEqual(report["judge"]["temperature"], OPENCODE_QWEN_TEMPERATURE)
            self.assertEqual(report["judge"]["enable_thinking"], OPENCODE_QWEN_ENABLE_THINKING)
            self.assertEqual(report["gate"]["successful_judgements"], 6)
            for profile in report["cases"][0]["profiles"].values():
                for result in (profile["judge"], *(trial["judge"] for trial in profile["trials"])):
                    self.assertEqual(result["model"], "qwen3.8-max")
                    self.assertEqual(result["label"], "qwen3.8-max-self-judge-v1")
            markdown = (root / "combined" / "report.md").read_text()
            self.assertIn("qwen3.8-max-self-judge-v1", markdown)
            self.assertNotIn("glm-5.2-self-judge-v1", markdown)
            if OPENCODE_QWEN_ENABLE_THINKING:
                self.assertIn("temperatures below 0.6", markdown)
                self.assertIn("documented provider behaviors", markdown)

    def test_unknown_judge_model_fails_before_loading_evidence(self) -> None:
        with self.assertRaisesRegex(SweQaError, "unsupported judge model"):
            judge_pairs(
                pairs_root=Path("missing"), references_path=Path("missing"),
                output_dir=Path("missing"), expected=["reflex:6"], model="unknown",
            )

    def test_qwen_judge_shared_credentials_fallback_strips_legacy_key(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            pairs_root, references = self._write_pair_and_reference(root)
            requests: list[dict[str, Any]] = []

            def completion(**kwargs: Any) -> dict[str, Any]:
                requests.append(kwargs)
                return {"choices": [{"message": {"content": json.dumps({key: 10 for key in (
                    "correctness", "completeness", "relevance", "clarity", "coherence"
                )})}}]}

            with patch.dict("os.environ", {
                "GLM_API_KEY": "  ", "OPENAI_API_KEY": " shared-key ",
                "OPENAI_BASE_URL": "https://shared.invalid/v1",
            }, clear=True):
                judge_pairs(
                    pairs_root=pairs_root, references_path=references,
                    output_dir=root / "report", expected=["reflex-6"],
                    completion_fn=completion, model="qwen3.8-max", attempts=1,
                )
            self.assertEqual(len(requests), 6)
            self.assertTrue(all(request["api_key"] == "shared-key" for request in requests))
            self.assertTrue(all(request["api_base"] == "https://shared.invalid/v1" for request in requests))

    def test_default_and_environment_judge_concurrency_are_bounded(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            pairs_root, references = self._write_pair_and_reference(root)

            for configured, expected_workers in ((None, 3), ("2", 2)):
                with self.subTest(configured=configured):
                    barrier = threading.Barrier(expected_workers)
                    lock = threading.Lock()
                    active = 0
                    max_active = 0

                    def fake_completion(**kwargs: Any) -> dict[str, Any]:
                        nonlocal active, max_active
                        with lock:
                            active += 1
                            max_active = max(max_active, active)
                        try:
                            barrier.wait(timeout=2)
                            content = json.dumps(
                                {
                                    key: 10
                                    for key in (
                                        "correctness",
                                        "completeness",
                                        "relevance",
                                        "clarity",
                                        "coherence",
                                    )
                                }
                            )
                            return {
                                "choices": [{"message": {"content": content}}],
                                "usage": {
                                    "prompt_tokens": 50,
                                    "completion_tokens": 5,
                                },
                                "_hidden_params": {"response_cost": 0.01},
                            }
                        finally:
                            with lock:
                                active -= 1

                    environment = {"GLM_API_KEY": "test-secret"}
                    if configured is not None:
                        environment["SWE_QA_JUDGE_CONCURRENCY"] = configured
                    with patch.dict("os.environ", environment, clear=True):
                        report = judge_pairs(
                            pairs_root=pairs_root,
                            references_path=references,
                            output_dir=root / f"report-{expected_workers}",
                            expected=["reflex-6"],
                            completion_fn=fake_completion,
                            attempts=1,
                        )

                    self.assertEqual(max_active, expected_workers)
                    self.assertEqual(report["judge"]["usage"]["calls"], 6)
                    for profile_name in ("baseline", "zvec-grep"):
                        self.assertEqual(
                            [
                                trial["trial_index"]
                                for trial in report["cases"][0]["profiles"][
                                    profile_name
                                ]["trials"]
                            ],
                            [1, 2, 3],
                        )

    def test_invalid_judge_concurrency_fails_before_model_call(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            pairs_root, references = self._write_pair_and_reference(root)

            for configured in (
                "",
                "0",
                "-1",
                "1.5",
                "many",
                str(MAX_JUDGE_CONCURRENCY + 1),
            ):
                with self.subTest(configured=configured):
                    called = False

                    def fake_completion(**kwargs: Any) -> dict[str, Any]:
                        nonlocal called
                        called = True
                        return {}

                    with patch.dict(
                        "os.environ",
                        {
                            "GLM_API_KEY": "test-secret",
                            "SWE_QA_JUDGE_CONCURRENCY": configured,
                        },
                        clear=True,
                    ):
                        with self.assertRaisesRegex(
                            SweQaError,
                            "must be an integer between 1 and "
                            f"{MAX_JUDGE_CONCURRENCY}",
                        ):
                            judge_pairs(
                                pairs_root=pairs_root,
                                references_path=references,
                                output_dir=root / "invalid-report",
                                expected=["reflex-6"],
                                completion_fn=fake_completion,
                                attempts=1,
                            )
                    self.assertFalse(called)

    def test_concurrent_failures_report_first_trial_in_output_order(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            pairs_root, references = self._write_pair_and_reference(root)
            later_failure_finished = threading.Event()

            def fake_completion(**kwargs: Any) -> dict[str, Any]:
                prompt_text = kwargs["messages"][0]["content"]
                if "Candidate answer:\nbaseline candidate 1" in prompt_text:
                    if not later_failure_finished.wait(timeout=2):
                        raise TimeoutError("later failure did not run")
                    raise LookupError("first trial failed later")
                if "Candidate answer:\nbaseline candidate 2" in prompt_text:
                    later_failure_finished.set()
                    raise ValueError("second trial failed first")
                content = json.dumps(
                    {
                        key: 10
                        for key in (
                            "correctness",
                            "completeness",
                            "relevance",
                            "clarity",
                            "coherence",
                        )
                    }
                )
                return {"choices": [{"message": {"content": content}}]}

            with patch.dict("os.environ", {"GLM_API_KEY": "test-secret"}, clear=True):
                with self.assertRaisesRegex(
                    SweQaError, r"transport error \(LookupError\)"
                ):
                    judge_pairs(
                        pairs_root=pairs_root,
                        references_path=references,
                        output_dir=root / "report",
                        expected=["reflex-6"],
                        completion_fn=fake_completion,
                        attempts=1,
                    )
            self.assertFalse((root / "report" / "report.json").exists())

    def test_aggregate_compares_displayed_totals_not_mean_percentages(self) -> None:
        def case(
            *,
            baseline: dict[str, int | float],
            zvec: dict[str, int | float],
            judge_baseline: int,
            judge_zvec: int,
            reductions: dict[str, float | None],
        ) -> dict[str, Any]:
            return {
                "task_id": f"task:{judge_baseline}",
                "profiles": {
                    "baseline": {
                        "judge": {"total": judge_baseline},
                        "metrics": baseline,
                    },
                    "zvec-grep": {
                        "judge": {"total": judge_zvec},
                        "metrics": zvec,
                    },
                },
                "comparison": {
                    "judge_delta": judge_zvec - judge_baseline,
                    **reductions,
                },
            }

        cases = [
            case(
                baseline={
                    "input_tokens": 100,
                    "tool_calls": 10,
                    "agent_wall_seconds": 10.0,
                    "cost_usd": 1.0,
                },
                zvec={
                    "input_tokens": 10,
                    "tool_calls": 1,
                    "agent_wall_seconds": 1.0,
                    "cost_usd": 0.1,
                },
                judge_baseline=50,
                judge_zvec=60,
                reductions={
                    "input_token_reduction_pct": 90.0,
                    "toolcall_reduction_pct": 90.0,
                    "time_reduction_pct": 90.0,
                    "cost_reduction_pct": 90.0,
                },
            ),
            case(
                baseline={
                    "input_tokens": 900,
                    "tool_calls": 90,
                    "agent_wall_seconds": 90.0,
                    "cost_usd": 9.0,
                },
                zvec={
                    "input_tokens": 900,
                    "tool_calls": 90,
                    "agent_wall_seconds": 90.0,
                    "cost_usd": 9.0,
                },
                judge_baseline=80,
                judge_zvec=70,
                reductions={
                    "input_token_reduction_pct": 0.0,
                    "toolcall_reduction_pct": 0.0,
                    "time_reduction_pct": 0.0,
                    "cost_reduction_pct": 0.0,
                },
            ),
        ]

        aggregate = _aggregate(cases)

        self.assertEqual(aggregate["comparison"]["judge_delta"], 0.0)
        totals_ratio = (1000 - 910) / 1000 * 100
        for key in (
            "input_token_reduction_pct", "toolcall_reduction_pct",
            "time_reduction_pct", "cost_reduction_pct",
        ):
            self.assertAlmostEqual(aggregate["comparison"][key], totals_ratio)
        self.assertEqual(
            aggregate["comparison_basis"], "ratio_of_aggregate_profile_means"
        )

        cases[1]["profiles"]["baseline"]["metrics"]["cost_usd"] = None
        aggregate = _aggregate(cases)
        self.assertIsNone(aggregate["comparison"]["cost_reduction_pct"])
        self.assertIsNone(aggregate["profiles"]["baseline"]["cost_usd"])
        self.assertEqual(
            aggregate["comparison_samples"]["cost_reduction_pct"], 0
        )
        cases[0]["profiles"]["baseline"]["metrics"]["cost_usd"] = None
        aggregate = _aggregate(cases)
        self.assertIsNone(aggregate["comparison"]["cost_reduction_pct"])
        self.assertEqual(
            aggregate["comparison_samples"]["cost_reduction_pct"], 0
        )

    def test_all_filtered_judged_task_remains_valid_for_offline_aggregation(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            pairs_root, references = self._write_pair_and_reference(root)
            pair_path = pairs_root / "pair-reflex-6.json"
            pair = json.loads(pair_path.read_text())
            for trial in pair["profiles"]["zvec-grep"]["trials"]:
                trial["input_tokens"] = 10000
            _write_json(pair_path, pair)
            calls: list[dict[str, Any]] = []

            def fake_completion(**kwargs: Any) -> dict[str, Any]:
                calls.append(kwargs)
                content = json.dumps({key: 18 for key in (
                    "correctness", "completeness", "relevance", "clarity", "coherence"
                )})
                return {
                    "choices": [{"message": {"content": content}}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 5},
                }

            with patch.dict("os.environ", {"GLM_API_KEY": "mock"}):
                source = judge_pairs(
                    pairs_root=pairs_root, references_path=references,
                    output_dir=root / "source", expected=["reflex-6"],
                    completion_fn=fake_completion,
                )
            self.assertEqual(len(calls), 6)
            combined = aggregate_reports(
                reports_root=root / "source", output_dir=root / "combined",
                expected=["reflex:6"],
            )
            for report in (source, combined):
                self.assertTrue(report["gate"]["passed"])
                self.assertEqual(report["gate"]["valid_pairs"], 1)
                self.assertEqual(report["gate"]["successful_judgements"], 6)
                self.assertEqual(report["judge"]["usage"]["calls"], 6)
                self.assertEqual(len(report["cases"]), 1)
                aggregate = report["aggregate"]
                self.assertEqual(aggregate["filter"]["included_count"], 0)
                self.assertEqual(aggregate["filter"]["excluded_count"], 1)
                self.assertTrue(all(value is None for value in aggregate["comparison"].values()))
                self.assertTrue(all(value == 0 for value in aggregate["comparison_samples"].values()))
                for profile in aggregate["profiles"].values():
                    for metric in ("judge", "input_tokens", "output_tokens", "tool_calls", "agent_wall_seconds", "cost_usd"):
                        self.assertIsNone(profile[metric])
            for directory in ("source", "combined"):
                markdown = (root / directory / "report.md").read_text()
                self.assertIn("| **Aggregate** | N/A | N/A | N/A | N/A |", markdown)
                self.assertNotIn("| reflex:6 |", markdown)
                self.assertIn("reflex:6", markdown)

    def test_judge_only_filtered_task_keeps_evidence_and_can_merge(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            pairs_root, references = self._write_pair_and_reference(root)
            calls: list[dict[str, Any]] = []

            def fake_completion(**kwargs: Any) -> dict[str, Any]:
                calls.append(kwargs)
                score = 15 if "Candidate answer:\nzvec-grep" in kwargs["messages"][0]["content"] else 10
                content = json.dumps({key: score for key in (
                    "correctness", "completeness", "relevance", "clarity", "coherence"
                )})
                return {
                    "choices": [{"message": {"content": content}}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 5},
                }

            with patch.dict("os.environ", {"GLM_API_KEY": "mock"}):
                source = judge_pairs(
                    pairs_root=pairs_root, references_path=references,
                    output_dir=root / "source", expected=["reflex-6"],
                    completion_fn=fake_completion,
                )
            combined = aggregate_reports(
                reports_root=root / "source", output_dir=root / "combined",
                expected=["reflex:6"],
            )
            self.assertEqual(len(calls), 6)
            for report in (source, combined):
                self.assertTrue(report["gate"]["passed"])
                self.assertEqual(report["gate"]["valid_pairs"], 1)
                self.assertEqual(report["gate"]["successful_judgements"], 6)
                self.assertEqual(report["judge"]["usage"]["calls"], 6)
                self.assertEqual(len(report["cases"]), 1)
                aggregate = report["aggregate"]
                self.assertEqual(aggregate["filter"]["included_count"], 0)
                self.assertEqual(aggregate["filter"]["excluded_count"], 1)
                self.assertEqual(aggregate["filter"]["excluded_tasks"][0]["reasons"], ["judge_delta_outside_range"])
                self.assertTrue(all(value is None for value in aggregate["comparison"].values()))
                self.assertTrue(all(value == 0 for value in aggregate["comparison_samples"].values()))
                for profile in aggregate["profiles"].values():
                    for metric in ("judge", "input_tokens", "output_tokens", "tool_calls", "agent_wall_seconds", "cost_usd"):
                        self.assertIsNone(profile[metric])
            for directory in ("source", "combined"):
                markdown = (root / directory / "report.md").read_text()
                main_table, excluded_section = markdown.split("### Tasks excluded for Judge differences", 1)
                self.assertIn("| **Aggregate** | N/A | N/A | N/A | N/A |", main_table)
                self.assertNotIn("| reflex:6 |", main_table)
                self.assertIn("| reflex:6 | 50.00 | 75.00 | +25.00 |", excluded_section)

    def test_missing_expected_pair_fails_before_model_call(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            pairs_root, references = self._write_pair_and_reference(root)
            called = False

            def fake_completion(**kwargs: Any) -> dict[str, Any]:
                nonlocal called
                called = True
                return {}

            with patch.dict("os.environ", {"GLM_API_KEY": "secret"}, clear=True):
                with self.assertRaisesRegex(SweQaError, "missing valid pair"):
                    judge_pairs(
                        pairs_root=pairs_root,
                        references_path=references,
                        output_dir=root / "report",
                        expected=["reflex-6", "sqlfluff-2"],
                        completion_fn=fake_completion,
                        attempts=1,
                    )
            self.assertFalse(called)

    def test_profile_trial_count_mismatch_fails_before_model_call(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            pairs_root, references = self._write_pair_and_reference(root)
            pair_path = pairs_root / "pair-reflex-6.json"
            pair = json.loads(pair_path.read_text())
            zvec = pair["profiles"]["zvec-grep"]
            zvec["trials"].pop()
            zvec["trial_count"] = 2
            _write_json(pair_path, pair)
            called = False

            def fake_completion(**kwargs: Any) -> dict[str, Any]:
                nonlocal called
                called = True
                return {}

            with patch.dict("os.environ", {"GLM_API_KEY": "secret"}, clear=True):
                with self.assertRaisesRegex(
                    SweQaError, "profile trial counts do not match"
                ):
                    judge_pairs(
                        pairs_root=pairs_root,
                        references_path=references,
                        output_dir=root / "report",
                        expected=["reflex-6"],
                        completion_fn=fake_completion,
                        attempts=1,
                    )
            self.assertFalse(called)


class AggregateReportTests(unittest.TestCase):
    def test_judge_filter_uses_trial_means_and_excludes_union_once(self) -> None:
        # Decimal means can subtract to just beyond 10 due to float rounding.
        # Both boundary directions remain included, while real 10.2-point
        # differences are excluded. A single divergent trial is insufficient.
        rows = [
            ("reflex:6", [54, 54, 54, 55, 55], [64, 64, 64, 65, 65], 100, 50),
            ("sqlfluff:2", [64, 64, 64, 65, 65], [54, 54, 54, 55, 55], 100, 50),
            ("conan:1", [60] * 5, [70, 70, 70, 70, 71], 100, 50),
            ("pylint:10", [70, 70, 70, 70, 71], [60] * 5, 100, 50),
            ("pylint:9", [60] * 5, [90, 55, 55, 55, 55], 100, 50),
            ("sympy:38", [60] * 5, [70, 70, 70, 70, 71], 100, 300),
            ("conan:39", [60] * 5, [60] * 5, 100, 300),
            ("xarray:46", [70, 70, 70, 70, 71], [60] * 5, 0, 1),
        ]
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            for index, (task_id, baseline, zvec, input_baseline, input_zvec) in enumerate(rows):
                source = _judged_task_report(task_id, index)
                _set_report_trial_judgements(source, baseline=baseline, zvec=zvec)
                _set_report_trial_metrics(
                    source,
                    baseline=[(input_baseline, 10, 10.0, 1.0)] * 5,
                    zvec=[(input_zvec, 4, 5.0, 0.5)] * 5,
                )
                _write_json(root / "reports" / str(index) / "report.json", source)
            report = aggregate_reports(
                reports_root=root / "reports", output_dir=root / "combined",
                expected=[row[0] for row in rows],
            )
            aggregate = report["aggregate"]
            filtering = aggregate["filter"]
            self.assertEqual(filtering["total_count"], 8)
            self.assertEqual(filtering["included_count"], 3)
            self.assertEqual(filtering["excluded_count"], 5)
            self.assertEqual(filtering["included_task_ids"], ["reflex:6", "sqlfluff:2", "pylint:9"])
            excluded = {row["task_id"]: row for row in filtering["excluded_tasks"]}
            self.assertEqual(set(excluded), {"conan:1", "pylint:10", "sympy:38", "conan:39", "xarray:46"})
            for task_id in ("conan:1", "pylint:10"):
                self.assertEqual(excluded[task_id]["reasons"], ["judge_delta_outside_range"])
            self.assertAlmostEqual(excluded["conan:1"]["judge_delta"], 10.2)
            self.assertAlmostEqual(excluded["pylint:10"]["judge_delta"], -10.2)
            self.assertEqual(excluded["sympy:38"]["reasons"], [
                "input_token_change_outside_range", "judge_delta_outside_range",
            ])
            self.assertEqual(excluded["conan:39"]["reasons"], ["input_token_change_outside_range"])
            self.assertEqual(excluded["xarray:46"]["reasons"], [
                "undefined_baseline", "judge_delta_outside_range",
            ])
            self.assertAlmostEqual(aggregate["profiles"]["baseline"]["judge"], (54.4 + 64.4 + 60) / 3)
            self.assertAlmostEqual(aggregate["profiles"]["zvec-grep"]["judge"], (64.4 + 54.4 + 62) / 3)
            self.assertEqual(aggregate["profiles"]["baseline"]["input_tokens"], 300)
            self.assertEqual(aggregate["profiles"]["zvec-grep"]["input_tokens"], 150)
            self.assertEqual(aggregate["profiles"]["baseline"]["tool_calls"], 30)
            self.assertEqual(aggregate["profiles"]["zvec-grep"]["tool_calls"], 12)
            self.assertEqual(aggregate["profiles"]["zvec-grep"]["agent_wall_seconds"], 15)
            self.assertEqual(aggregate["profiles"]["zvec-grep"]["cost_usd"], 1.5)
            self.assertEqual(report["gate"]["valid_pairs"], 8)
            self.assertEqual(report["gate"]["successful_judgements"], 80)
            self.assertEqual(report["judge"]["usage"]["calls"], 80)
            self.assertEqual(len(report["cases"]), 8)
            markdown = (root / "combined" / "report.md").read_text()
            main_table, excluded_section = markdown.split("### Tasks excluded for Judge differences", 1)
            self.assertIn("| **Aggregate** |", main_table)
            self.assertLess(main_table.index("| **Aggregate** |"), main_table.index("| reflex:6 |"))
            for task_id in excluded:
                self.assertNotIn(f"| {task_id} |", main_table)
            self.assertIn("| Task | Baseline Judge | zvec-grep Judge | Judge change (points) | Exclusion reason |", excluded_section)
            self.assertIn("| conan:1 | 60.00 | 70.20 | +10.20 |", excluded_section)
            self.assertIn("| pylint:10 | 70.20 | 60.00 | -10.20 |", excluded_section)
            self.assertIn("| sympy:38 | 60.00 | 70.20 | +10.20 |", excluded_section)
            self.assertIn("| xarray:46 | 70.20 | 60.00 | -10.20 |", excluded_section)
            self.assertNotIn("| conan:39 |", excluded_section)

    def test_input_filter_uses_task_means_and_keeps_boundary_values(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            # +100% and -100% are retained. A zero/zero input pair is also
            # retained, while positive input against a zero baseline is not.
            rows = [
                ("reflex:6", [100, 100, 100], [200, 200, 200]),
                ("sqlfluff:2", [100, 100, 100], [201, 201, 201]),
                ("conan:1", [100, 100, 100], [0, 0, 0]),
                ("pylint:10", [0, 0, 0], [0, 0, 0]),
                ("pylint:9", [0, 0, 0], [1, 1, 1]),
                # One trial rises 400%, but the task mean rises only 66.67%.
                ("sympy:38", [100, 100, 100], [500, 0, 0]),
            ]
            for index, (task_id, baseline, zvec) in enumerate(rows):
                source = _judged_task_report(task_id, index)
                _set_report_trial_metrics(
                    source,
                    baseline=[(value, 10, 10.0, 1.0) for value in baseline],
                    zvec=[(value, 1000, 1000.0, 100.0) for value in zvec],
                )
                _write_json(root / "reports" / str(index) / "report.json", source)

            report = aggregate_reports(
                reports_root=root / "reports", output_dir=root / "combined",
                expected=[row[0] for row in rows],
            )
            aggregate = report["aggregate"]
            filtering = aggregate["filter"]
            self.assertEqual(filtering["criteria"], [
                {"metric": "input_tokens", "comparison": "change_pct", "min": -100.0, "max": 100.0},
                {"metric": "judge", "comparison": "delta", "min": -10.0, "max": 10.0, "unit": "points"},
            ])
            self.assertEqual(filtering["total_count"], 6)
            self.assertEqual(filtering["included_count"], 4)
            self.assertEqual(filtering["excluded_count"], 2)
            included = ["reflex:6", "conan:1", "pylint:10", "sympy:38"]
            self.assertEqual(filtering["included_task_ids"], included)
            self.assertEqual(filtering["excluded_tasks"], [
                {
                    "task_id": "sqlfluff:2", "baseline_input_tokens": 100.0,
                    "zvec_grep_input_tokens": 201.0, "change_pct": 101.0,
                    "baseline_judge": 55.0, "zvec_grep_judge": 65.0, "judge_delta": 10.0,
                    "reasons": ["input_token_change_outside_range"],
                },
                {
                    "task_id": "pylint:9", "baseline_input_tokens": 0.0,
                    "zvec_grep_input_tokens": 1.0, "change_pct": None,
                    "baseline_judge": 70.0, "zvec_grep_judge": 80.0, "judge_delta": 10.0,
                    "reasons": ["undefined_baseline"],
                },
            ])
            baseline = aggregate["profiles"]["baseline"]
            zvec = aggregate["profiles"]["zvec-grep"]
            self.assertEqual(baseline["input_tokens"], 300)
            self.assertAlmostEqual(zvec["input_tokens"], 200 + 500 / 3)
            self.assertEqual(baseline["judge"], (50 + 60 + 65 + 50) / 4)
            self.assertEqual(zvec["judge"], (60 + 70 + 75 + 60) / 4)
            self.assertEqual(baseline["tool_calls"], 40)
            self.assertEqual(zvec["tool_calls"], 4000)
            self.assertEqual(zvec["agent_wall_seconds"], 4000)
            self.assertEqual(zvec["cost_usd"], 400)
            self.assertAlmostEqual(aggregate["comparison"]["input_token_reduction_pct"], -200 / 9)
            self.assertEqual(report["gate"]["valid_pairs"], 6)
            self.assertEqual(report["gate"]["successful_judgements"], 36)
            self.assertEqual(report["judge"]["usage"]["calls"], 36)
            self.assertEqual([case["task_id"] for case in report["cases"]], [row[0] for row in rows])
            markdown = (root / "combined" / "report.md").read_text()
            for task_id in included:
                self.assertIn(f"| {task_id} |", markdown)
                self.assertLess(markdown.index("| **Aggregate** |"), markdown.index(f"| {task_id} |"))
            for task_id in ("sqlfluff:2", "pylint:9"):
                self.assertNotIn(f"| {task_id} |", markdown)
                self.assertIn(task_id, markdown)

    def test_aggregate_accepts_qwen_and_rejects_mixed_or_inconsistent_identities(self) -> None:
        def qwen_report(task_id: str, index: int) -> dict[str, Any]:
            report = _judged_task_report(task_id, index)
            identities = [report["judge"]]
            for profile in report["cases"][0]["profiles"].values():
                identities.append(profile["judge"])
                identities.extend(trial["judge"] for trial in profile["trials"])
            for identity in identities:
                identity.update(model="qwen3.8-max", label="qwen3.8-max-self-judge-v1")
            report["judge"]["temperature"] = OPENCODE_QWEN_TEMPERATURE
            return report

        for mismatch in (None, "mixed_models", "label", "profile", "trial", "unknown", "temperature"):
            with self.subTest(mismatch=mismatch), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                first = qwen_report("reflex:6", 0)
                second = qwen_report("sqlfluff:2", 1)
                if mismatch == "mixed_models":
                    second = _judged_task_report("sqlfluff:2", 1)
                elif mismatch == "label":
                    second["judge"]["label"] = SELF_JUDGE_LABEL
                elif mismatch in ("profile", "trial"):
                    profile = second["cases"][0]["profiles"]["baseline"]
                    identity = profile["judge"] if mismatch == "profile" else profile["trials"][0]["judge"]
                    identity.update(model="glm-5.2", label=SELF_JUDGE_LABEL)
                elif mismatch == "unknown":
                    second["judge"].update(model="unknown", label="unknown-self-judge-v1")
                elif mismatch == "temperature":
                    second["judge"]["temperature"] = OPENCODE_QWEN_TEMPERATURE + 0.5
                _write_json(root / "reports" / "first" / "report.json", first)
                _write_json(root / "reports" / "second" / "report.json", second)
                if mismatch is None:
                    report = aggregate_reports(reports_root=root / "reports", output_dir=root / "combined")
                    self.assertEqual(report["judge"]["model"], "qwen3.8-max")
                    self.assertEqual(report["judge"]["usage"]["calls"], 12)
                else:
                    with self.assertRaisesRegex(SweQaError, "incompatible judge"):
                        aggregate_reports(reports_root=root / "reports", output_dir=root / "combined")

    def test_aggregate_preserves_generation_metadata_and_rejects_mixing(self) -> None:
        variants = [
            {},
            {"enable_thinking": False},
            {"reasoning_effort": "medium"},
            {"reasoning_effort": None},
            {"max_tokens": 16000},
            {"response_format": {"type": "json_object"}},
            None,
        ]
        for overrides in variants:
            for legacy_first in (False, True):
                with (
                    self.subTest(overrides=overrides, legacy_first=legacy_first),
                    tempfile.TemporaryDirectory() as temp_dir,
                ):
                    root = Path(temp_dir)
                    first = _judged_task_report("reflex:6")
                    second = _judged_task_report("sqlfluff:2", 1)
                    first["judge"].update(
                        copy.deepcopy(JUDGE_GENERATION_METADATA)
                    )
                    if overrides is not None:
                        second["judge"].update(
                            copy.deepcopy(JUDGE_GENERATION_METADATA)
                        )
                        second["judge"].update(overrides)
                    if legacy_first:
                        first, second = second, first
                    _write_json(root / "reports" / "first" / "report.json", first)
                    _write_json(root / "reports" / "second" / "report.json", second)
                    if overrides == {}:
                        report = aggregate_reports(
                            reports_root=root / "reports",
                            output_dir=root / "combined",
                        )
                        persisted = json.loads(
                            (root / "combined" / "report.json").read_text()
                        )
                        for key, value in JUDGE_GENERATION_METADATA.items():
                            self.assertEqual(report["judge"][key], value)
                            self.assertEqual(persisted["judge"][key], value)
                    else:
                        with self.assertRaisesRegex(
                            SweQaError, "incompatible judge metadata"
                        ):
                            aggregate_reports(
                                reports_root=root / "reports",
                                output_dir=root / "combined",
                            )

    def test_aggregate_preserves_explicit_thinking_false_and_unknown_legacy(self) -> None:
        for metadata in (
            {},
            {**JUDGE_GENERATION_METADATA,
             "enable_thinking": False, "reasoning_effort": None},
            {**JUDGE_GENERATION_METADATA,
             "enable_thinking": False, "reasoning_effort": None,
             "response_format": {"type": "json_object"}},
        ):
            with (
                self.subTest(metadata=metadata),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                for index, task_id in enumerate(("reflex:6", "sqlfluff:2")):
                    report = _judged_task_report(task_id, index)
                    report["judge"].update(copy.deepcopy(metadata))
                    _write_json(root / "reports" / str(index) / "report.json", report)
                combined = aggregate_reports(
                    reports_root=root / "reports", output_dir=root / "combined"
                )
                for key in JUDGE_GENERATION_METADATA:
                    if metadata:
                        self.assertEqual(combined["judge"][key], metadata[key])
                    else:
                        self.assertNotIn(key, combined["judge"])

    def test_aggregate_rejects_partial_or_invalid_generation_metadata(self) -> None:
        invalid: list[dict[str, Any]] = []
        for key, value in JUDGE_GENERATION_METADATA.items():
            invalid.append({key: value})
            invalid.append({
                name: item for name, item in JUDGE_GENERATION_METADATA.items()
                if name != key
            })
        for key, values in (
            ("enable_thinking", (None, 0, 1, "true")),
            ("reasoning_effort", ("", True, 4)),
            ("max_tokens", (None, True, 0, -1, 32000.0, "32000")),
            ("response_format", (
                "json_object", {}, {"type": "text"},
                {"type": "json_object", "other": True},
            )),
        ):
            invalid.extend(
                {**JUDGE_GENERATION_METADATA, key: value} for value in values
            )
        for metadata in invalid:
            with (
                self.subTest(metadata=metadata),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                report = _judged_task_report("reflex:6")
                report["judge"].update(copy.deepcopy(metadata))
                _write_json(root / "reports" / "report.json", report)
                with self.assertRaises(SweQaError):
                    aggregate_reports(
                        reports_root=root / "reports", output_dir=root / "combined"
                    )

    def test_aggregate_preserves_seed_and_rejects_mixed_sampling(self) -> None:
        for second_seed in (42, 7, None):
            with (
                self.subTest(second_seed=second_seed),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                first = _judged_task_report("reflex:6")
                first["judge"]["seed"] = 42
                second = _judged_task_report("sqlfluff:2", 1)
                if second_seed is not None:
                    second["judge"]["seed"] = second_seed
                _write_json(root / "reports" / "first" / "report.json", first)
                _write_json(root / "reports" / "second" / "report.json", second)
                if second_seed == 42:
                    report = aggregate_reports(
                        reports_root=root / "reports", output_dir=root / "combined"
                    )
                    self.assertEqual(report["judge"]["seed"], 42)
                else:
                    with self.assertRaisesRegex(
                        SweQaError, "incompatible judge metadata"
                    ):
                        aggregate_reports(
                            reports_root=root / "reports",
                            output_dir=root / "combined",
                        )

    def test_aggregate_rejects_invalid_seed_metadata(self) -> None:
        for seed in (True, 42.5, "42", None):
            with self.subTest(seed=seed), tempfile.TemporaryDirectory() as temp_dir:
                root = Path(temp_dir)
                report = _judged_task_report("reflex:6")
                report["judge"]["seed"] = seed
                _write_json(root / "reports" / "report.json", report)
                with self.assertRaisesRegex(SweQaError, "invalid judge seed"):
                    aggregate_reports(
                        reports_root=root / "reports", output_dir=root / "combined"
                    )

    def test_cli_aggregates_single_report_without_glm_credentials(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            reports_root = root / "reports"
            output_dir = reports_root / "combined"
            _write_json(
                reports_root / "reflex-6" / "report.json",
                _judged_task_report("reflex:6"),
            )

            with (
                patch.dict("os.environ", {}, clear=True),
                patch("builtins.print") as print_mock,
            ):
                exit_code = swe_qa_main(
                    [
                        "aggregate",
                        "--reports-root",
                        str(reports_root),
                        "--output-dir",
                        str(output_dir),
                        "--expected",
                        "reflex:6",
                    ]
                )

            self.assertEqual(exit_code, 0)
            self.assertEqual(print_mock.call_count, 1)
            report = json.loads((output_dir / "report.json").read_text())
            self.assertEqual(
                [case["task_id"] for case in report["cases"]], ["reflex:6"]
            )
            self.assertEqual(report["gate"]["expected_tasks"], ["reflex:6"])
            self.assertEqual(report["gate"]["successful_judgements"], 6)
            self.assertEqual(report["judge"]["usage"]["calls"], 6)
            self.assertIn("| reflex:6 |", (output_dir / "report.md").read_text())

            # A retry may scan a root that already contains its own prior output.
            # The aggregate output is excluded instead of becoming a source report.
            with patch.dict("os.environ", {}, clear=True):
                retried = aggregate_reports(
                    reports_root=reports_root,
                    output_dir=output_dir,
                )
            self.assertEqual(len(retried["cases"]), 1)

    def test_offline_aggregate_compares_sums_of_task_means(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            reports_root = root / "reports"
            first = _judged_task_report("reflex:6")
            _set_report_trial_metrics(
                first,
                baseline=[
                    (100, 10, 10.0, 1.0),
                    (900, 90, 90.0, 9.0),
                    (100, 10, 20.0, 2.0),
                ],
                zvec=[
                    (10, 1, 1.0, 0.1),
                    (900, 90, 90.0, 9.0),
                    (50, 5, 10.0, 1.0),
                ],
            )
            first["cases"][0]["comparison"][
                "input_token_reduction_pct"
            ] = 140 / 3
            # Pair by trial_index, not incidental array order in the artifact.
            first["cases"][0]["profiles"]["zvec-grep"]["trials"].reverse()

            second = _judged_task_report("sqlfluff:2", 1)
            _set_report_trial_metrics(
                second,
                baseline=[
                    (1000, 100, 100.0, 10.0),
                    (1000, 100, 100.0, 10.0),
                    (1000, 100, 100.0, 10.0),
                ],
                zvec=[
                    (500, 50, 50.0, 5.0),
                    (500, 50, 50.0, 5.0),
                    (500, 50, 50.0, 5.0),
                ],
            )
            _write_json(reports_root / "first" / "report.json", first)
            _write_json(reports_root / "second" / "report.json", second)

            report = aggregate_reports(
                reports_root=reports_root,
                output_dir=root / "combined",
                expected=["reflex:6", "sqlfluff:2"],
            )

            first_comparison = report["cases"][0]["comparison"]
            self.assertAlmostEqual(
                first_comparison["input_token_reduction_pct"],
                (1100 - 960) / 1100 * 100,
            )
            self.assertEqual(
                [row["trial_index"] for row in first_comparison["trials"]],
                [1, 2, 3],
            )
            self.assertEqual(
                [
                    row["input_token_reduction_pct"]
                    for row in first_comparison["trials"]
                ],
                [90.0, 0.0, 50.0],
            )

            aggregate = report["aggregate"]
            self.assertAlmostEqual(
                aggregate["comparison"]["input_token_reduction_pct"],
                40.0,
            )
            self.assertEqual(
                aggregate["comparison_samples"][
                    "input_token_reduction_pct"
                ],
                2,
            )
            self.assertAlmostEqual(
                aggregate["profiles"]["baseline"]["input_tokens"],
                1100 / 3 + 1000,
            )
            self.assertEqual(
                aggregate["profiles"]["zvec-grep"]["input_tokens"], 820.0
            )
            grand_totals_ratio = (
                aggregate["profiles"]["baseline"]["input_tokens"]
                - aggregate["profiles"]["zvec-grep"]["input_tokens"]
            ) / aggregate["profiles"]["baseline"]["input_tokens"] * 100
            self.assertAlmostEqual(grand_totals_ratio, 40.0)
            self.assertAlmostEqual(
                aggregate["comparison"]["input_token_reduction_pct"],
                grand_totals_ratio,
            )
            markdown = (root / "combined" / "report.md").read_text()
            self.assertIn("1,366.67 / 820.00 / -40.00%", markdown)

    def test_zero_baseline_metric_still_contributes_to_aggregate_totals(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            source = _judged_task_report("reflex:6")
            _set_report_trial_metrics(
                source,
                baseline=[
                    (100, 0, 10.0, 0.0),
                    (100, 0, 10.0, 1.0),
                    (100, 0, 10.0, None),
                ],
                zvec=[
                    (50, 1, 5.0, 0.1),
                    (50, 1, 5.0, 0.5),
                    (50, 1, 5.0, None),
                ],
            )
            _write_json(root / "reports" / "source" / "report.json", source)
            _write_json(
                root / "reports" / "valid" / "report.json",
                _judged_task_report("sqlfluff:2", 1),
            )

            report = aggregate_reports(
                reports_root=root / "reports",
                output_dir=root / "combined",
                expected=["reflex:6", "sqlfluff:2"],
            )

            comparison = report["cases"][0]["comparison"]
            self.assertIsNone(
                comparison["trials"][0]["toolcall_reduction_pct"]
            )
            self.assertIsNone(comparison["toolcall_reduction_pct"])
            self.assertIsNone(comparison["cost_reduction_pct"])
            self.assertAlmostEqual(
                report["aggregate"]["comparison"]["toolcall_reduction_pct"],
                55.0,
            )
            self.assertIsNone(report["aggregate"]["comparison"]["cost_reduction_pct"])
            self.assertEqual(
                report["aggregate"]["profiles"]["baseline"]["tool_calls"],
                20.0,
            )
            self.assertEqual(
                report["aggregate"]["comparison_samples"][
                    "toolcall_reduction_pct"
                ],
                2,
            )
            self.assertEqual(
                report["aggregate"]["comparison_samples"][
                    "cost_reduction_pct"
                ],
                0,
            )
            markdown = (root / "combined" / "report.md").read_text()
            self.assertIn("0.00 / 1.00 / N/A", markdown)
            self.assertIn("20.00 / 9.00 / -55.00%", markdown)
            self.assertIn("toolcall n=2/2", markdown)

    def test_filters_summary_without_relaxing_twenty_task_completion_gate(self) -> None:
        tasks = list(EXPECTED_TASK_IDS)
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            reports_root = root / "reports"
            for index, task_id in enumerate(tasks):
                source = _judged_task_report(task_id, index)
                profile = source["cases"][0]["profiles"]
                baseline_score = int(profile["baseline"]["judge"]["total"])
                zvec_score = int(profile["zvec-grep"]["judge"]["total"])
                _set_report_trial_judgements(
                    source, baseline=[baseline_score] * 5,
                    zvec=[baseline_score + 15 if task_id == "pylint:10" else zvec_score] * 5,
                )
                if task_id == "requests:16":
                    _set_report_trial_metrics(
                        source,
                        baseline=[(1500, 150, 300.0, 3.0)] * 5,
                        zvec=[(6000, 60, 150.0, 1.5)] * 5,
                    )
                _write_json(
                    reports_root / f"artifact-{index}" / "report.json",
                    source,
                )

            with patch.dict("os.environ", {}, clear=True):
                report = aggregate_reports(
                    reports_root=reports_root,
                    output_dir=root / "combined",
                    expected=tasks,
                )

            self.assertEqual([case["task_id"] for case in report["cases"]], tasks)
            self.assertEqual(report["gate"]["expected_tasks"], tasks)
            self.assertEqual(report["gate"]["valid_pairs"], 20)
            self.assertEqual(report["gate"]["successful_judgements"], 200)
            self.assertEqual(report["judge"]["usage"]["calls"], 200)
            self.assertEqual(
                report["judge"]["usage"]["input_tokens"],
                sum(500 * (index + 1) for index in range(20)),
            )
            self.assertEqual(
                report["aggregate"]["profiles"]["baseline"]["input_tokens"],
                19100,
            )
            self.assertEqual(
                report["aggregate"]["comparison"]["input_token_reduction_pct"],
                50.0,
            )
            markdown = (root / "combined" / "report.md").read_text()
            main_table, excluded_section = markdown.split("### Tasks excluded for Judge differences", 1)
            self.assertIn("/ -50.00%", markdown)
            for task_id in tasks:
                if task_id in ("requests:16", "pylint:10"):
                    self.assertNotIn(f"| {task_id} |", main_table)
                    self.assertIn(task_id, markdown)
                else:
                    self.assertIn(f"| {task_id} |", main_table)
            self.assertIn("| pylint:10 | 65.00 | 80.00 | +15.00 |", excluded_section)
            self.assertIn("| **Aggregate** |", markdown)
            self.assertLess(markdown.index("| **Aggregate** |"), markdown.index("| reflex:6 |"))
            self.assertEqual(report["aggregate"]["filter"]["total_count"], 20)
            self.assertEqual(report["aggregate"]["filter"]["included_count"], 18)
            self.assertEqual(report["aggregate"]["filter"]["excluded_count"], 2)

    def test_aggregate_rejects_missing_expected_task(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            reports_root = root / "reports"
            _write_json(
                reports_root / "reflex-6" / "report.json",
                _judged_task_report("reflex:6"),
            )

            with self.assertRaisesRegex(SweQaError, "aggregate report task mismatch"):
                aggregate_reports(
                    reports_root=reports_root,
                    output_dir=root / "combined",
                    expected=["reflex:6", "sqlfluff:2"],
                )

    def test_aggregate_rejects_zero_reports(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            reports_root = root / "reports"
            reports_root.mkdir()

            with self.assertRaisesRegex(
                SweQaError, "no per-task report.json files found"
            ):
                aggregate_reports(
                    reports_root=reports_root,
                    output_dir=root / "combined",
                )

    def test_aggregate_rejects_duplicate_task_reports(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            source = _judged_task_report("reflex:6")
            _write_json(root / "reports" / "one" / "report.json", source)
            _write_json(root / "reports" / "two" / "report.json", source)

            with self.assertRaisesRegex(
                SweQaError, "duplicate task report for reflex:6"
            ):
                aggregate_reports(
                    reports_root=root / "reports",
                    output_dir=root / "combined",
                )


class ValidationTests(unittest.TestCase):
    def test_checked_in_selection_references_and_dataset_validate(self) -> None:
        result = validate_assets(
            selection_path=SELECTION_PATH,
            references_path=REFERENCES_PATH,
            dataset_path=DATASET_PATH,
        )

        self.assertTrue(result["valid"])
        self.assertEqual(result["task_count"], 20)
        self.assertEqual(tuple(result["task_ids"]), EXPECTED_TASK_IDS)
        self.assertTrue(result["references_are_judge_only"])

    def test_reference_answer_leak_in_dataset_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            copied_dataset = Path(temp_dir) / "dataset"
            shutil.copytree(DATASET_PATH, copied_dataset)
            references = json.loads(REFERENCES_PATH.read_text())
            leaked = references["references"][0]["reference_answer"]
            (copied_dataset / "reflex-6" / "leak.txt").write_text(leaked)

            with self.assertRaisesRegex(SweQaError, "leaked into Harbor dataset"):
                validate_assets(
                    selection_path=SELECTION_PATH,
                    references_path=REFERENCES_PATH,
                    dataset_path=copied_dataset,
                )

    def test_question_hash_mismatch_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            selection = json.loads(SELECTION_PATH.read_text())
            selection["tasks"][0]["question_hash"] = "0" * 64
            selection_path = Path(temp_dir) / "selection.json"
            _write_json(selection_path, selection)

            with self.assertRaisesRegex(SweQaError, "SHA256 mismatch"):
                validate_assets(
                    selection_path=selection_path,
                    references_path=REFERENCES_PATH,
                    dataset_path=DATASET_PATH,
                )

    def test_source_index_mismatch_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            selection = json.loads(SELECTION_PATH.read_text())
            selection["tasks"][0]["source_index"] = 7
            selection_path = Path(temp_dir) / "selection.json"
            _write_json(selection_path, selection)

            with self.assertRaisesRegex(SweQaError, "source index"):
                validate_assets(
                    selection_path=selection_path,
                    references_path=REFERENCES_PATH,
                    dataset_path=DATASET_PATH,
                )

    def test_category_distribution_mismatch_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            selection = json.loads(SELECTION_PATH.read_text())
            selection["tasks"][4]["category"] = "where"
            selection["tasks"][4]["question_type"] = "where"
            selection_path = Path(temp_dir) / "selection.json"
            _write_json(selection_path, selection)

            with self.assertRaisesRegex(SweQaError, "5 tasks in each"):
                validate_assets(
                    selection_path=selection_path,
                    references_path=REFERENCES_PATH,
                    dataset_path=DATASET_PATH,
                )

    def test_gate_category_task_list_mismatch_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            selection = json.loads(SELECTION_PATH.read_text())
            selection["gate"]["category_tasks"].pop()
            selection_path = Path(temp_dir) / "selection.json"
            _write_json(selection_path, selection)

            with self.assertRaisesRegex(SweQaError, "all non-smoke tasks"):
                validate_assets(
                    selection_path=selection_path,
                    references_path=REFERENCES_PATH,
                    dataset_path=DATASET_PATH,
                )


if __name__ == "__main__":
    unittest.main()
