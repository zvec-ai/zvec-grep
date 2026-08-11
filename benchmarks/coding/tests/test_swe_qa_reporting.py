from __future__ import annotations

import json
import shutil
import tempfile
import threading
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

from zg_bench.swe_qa import SELF_JUDGE_LABEL, SweQaError
from zg_bench.swe_qa.cli import main as swe_qa_main
from zg_bench.swe_qa.collect import collect_pair
from zg_bench.swe_qa.judge import (
    MAX_JUDGE_CONCURRENCY,
    _aggregate,
    aggregate_reports,
    judge_pairs,
)
from zg_bench.swe_qa.validation import validate_assets

CODING_DIR = Path(__file__).resolve().parents[1]
SELECTION_PATH = CODING_DIR / "zg_bench" / "swe_qa" / "data" / "selection.json"
REFERENCES_PATH = CODING_DIR / "zg_bench" / "swe_qa" / "data" / "references.json"
DATASET_PATH = CODING_DIR / "datasets" / "swe-qa-bench-manual"


def _write_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value), encoding="utf-8")


def _judged_task_report(task_id: str, index: int = 0) -> dict[str, Any]:
    scale = index + 1
    baseline_score = 10 + index
    zvec_score = 12 + index
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
        "role": "smoke" if index == 0 else "what",
        "category": "smoke" if index == 0 else "what",
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


def _write_harbor_job(
    root: Path,
    *,
    profile: str,
    trials: list[dict[str, Any]],
) -> None:
    job_dir = root / f"manual-reflex-6-{profile}"
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


class JudgeTests(unittest.TestCase):
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
                    "zvec-grep candidate 1": 15,
                    "zvec-grep candidate 2": 16,
                    "zvec-grep candidate 3": 17,
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
            self.assertTrue(
                all(call["model"] == "openai/glm-5.2" for call in requests)
            )
            self.assertTrue(
                all(call["api_key"] == "test-secret" for call in requests)
            )
            self.assertEqual(report["schema_version"], 2)
            self.assertEqual(report["judge"]["label"], SELF_JUDGE_LABEL)
            self.assertTrue(report["judge"]["self_judge"])
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
            self.assertEqual(zvec["judge"]["scores"]["correctness"], 16.0)
            self.assertEqual(zvec["judge"]["total"], 80.0)
            self.assertEqual(
                [trial["judge"]["total"] for trial in baseline["trials"]],
                [50, 60, 70],
            )
            self.assertEqual(
                [trial["judge"]["total"] for trial in zvec["trials"]],
                [75, 80, 85],
            )
            self.assertAlmostEqual(
                baseline["metrics"]["input_tokens"], 1100 / 3
            )
            self.assertEqual(zvec["metrics"]["input_tokens"], 320.0)

            comparison = case["comparison"]
            self.assertEqual(comparison["judge_delta"], 20.0)
            self.assertEqual(
                [trial["trial_index"] for trial in comparison["trials"]],
                [1, 2, 3],
            )
            self.assertAlmostEqual(
                comparison["input_token_reduction_pct"], 140 / 3
            )
            self.assertAlmostEqual(
                comparison["toolcall_reduction_pct"], 140 / 3
            )
            self.assertAlmostEqual(comparison["time_reduction_pct"], 140 / 3)
            profile_mean_ratio = (1100 / 3 - 320) / (1100 / 3) * 100
            self.assertNotAlmostEqual(
                comparison["input_token_reduction_pct"], profile_mean_ratio
            )
            markdown = (output_dir / "report.md").read_text()
            self.assertIn("Aggregate", markdown)
            self.assertIn("input_token", markdown)
            self.assertIn("60.00 / 80.00 / +20.00", markdown)
            self.assertIn("366.67 / 320.00 / +46.67%", markdown)
            self.assertIn("equal-weight arithmetic mean", markdown)
            self.assertIn("not a ratio of totals", markdown)
            self.assertNotIn("cost", markdown.lower())
            self.assertNotIn("$", markdown)
            self.assertEqual(summary.read_text(), markdown)
            serialized = (output_dir / "report.json").read_text()
            self.assertNotIn("judge-only reference", serialized)
            self.assertNotIn("test-secret", serialized)

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

    def test_aggregate_averages_per_case_reductions_without_weighting(self) -> None:
        def case(
            *,
            baseline: dict[str, int | float],
            zvec: dict[str, int | float],
            judge_baseline: int,
            judge_zvec: int,
            reductions: dict[str, float | None],
        ) -> dict[str, Any]:
            return {
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
                judge_zvec=70,
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

        self.assertEqual(aggregate["comparison"]["judge_delta"], 5.0)
        self.assertEqual(aggregate["comparison"]["input_token_reduction_pct"], 45.0)
        self.assertEqual(aggregate["comparison"]["toolcall_reduction_pct"], 45.0)
        self.assertEqual(aggregate["comparison"]["time_reduction_pct"], 45.0)
        self.assertEqual(aggregate["comparison"]["cost_reduction_pct"], 45.0)
        totals_ratio = (1000 - 910) / 1000 * 100
        self.assertNotEqual(
            aggregate["comparison"]["input_token_reduction_pct"], totals_ratio
        )

        cases[1]["comparison"]["cost_reduction_pct"] = None
        self.assertIsNone(_aggregate(cases)["comparison"]["cost_reduction_pct"])

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

    def test_aggregates_five_present_reports(self) -> None:
        tasks = [
            "sympy:38",
            "reflex:6",
            "streamlink:14",
            "sqlfluff:2",
            "pylint:25",
        ]
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            reports_root = root / "reports"
            for index, task_id in enumerate(tasks):
                _write_json(
                    reports_root / f"artifact-{index}" / "report.json",
                    _judged_task_report(task_id, index),
                )

            with patch.dict("os.environ", {}, clear=True):
                report = aggregate_reports(
                    reports_root=reports_root,
                    output_dir=root / "combined",
                )

            expected_tasks = sorted(tasks)
            self.assertEqual(
                [case["task_id"] for case in report["cases"]], expected_tasks
            )
            self.assertEqual(report["gate"]["expected_tasks"], expected_tasks)
            self.assertEqual(report["gate"]["valid_pairs"], 5)
            self.assertEqual(report["gate"]["successful_judgements"], 30)
            self.assertEqual(report["judge"]["usage"]["calls"], 30)
            self.assertEqual(
                report["judge"]["usage"]["input_tokens"],
                sum(300 * (index + 1) for index in range(5)),
            )
            self.assertEqual(
                report["aggregate"]["profiles"]["baseline"]["input_tokens"],
                1500,
            )
            self.assertEqual(
                report["aggregate"]["comparison"]["input_token_reduction_pct"],
                50.0,
            )
            markdown = (root / "combined" / "report.md").read_text()
            for task_id in tasks:
                self.assertIn(f"| {task_id} |", markdown)
            self.assertIn("| **Aggregate** |", markdown)

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
        self.assertEqual(result["task_count"], 5)
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


if __name__ == "__main__":
    unittest.main()
