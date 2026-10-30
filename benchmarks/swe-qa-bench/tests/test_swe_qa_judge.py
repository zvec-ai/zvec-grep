"""Self-judge request, concurrency and suite orchestration contracts."""

from __future__ import annotations

import copy
import json
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from unittest.mock import patch

from swe_qa_fixtures import (
    JUDGE_GENERATION_METADATA,
    _write_json,
)
from swe_qa_fixtures import (
    setUpModule as setUpModule,
)
from swe_qa_fixtures import (
    tearDownModule as tearDownModule,
)
from zg_bench.engines.judge import (
    MAX_JUDGE_CONCURRENCY,
    default_completion,
    judge_candidate,
)
from zg_bench.reports.aggregate import aggregate_reports
from zg_bench.settings import (
    OPENCODE_QWEN_ENABLE_THINKING,
    OPENCODE_QWEN_REASONING_EFFORT,
    OPENCODE_QWEN_TEMPERATURE,
)
from zg_bench.swe_qa import SELF_JUDGE_LABEL, SweQaError
from zg_bench.swe_qa.cli import main as swe_qa_main
from zg_bench.swe_qa.judge import judge_pairs


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
            self.assertTrue(all(call["model"] == "openai/glm-5.2" for call in requests))
            self.assertTrue(all(call["api_key"] == "test-secret" for call in requests))
            self.assertEqual(report["schema_version"], 4)
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
            self.assertAlmostEqual(baseline["metrics"]["input_tokens"], 1100 / 3)
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
                [trial["input_token_reduction_pct"] for trial in comparison["trials"]],
                [90.0, 0.0, 50.0],
            )
            markdown = (output_dir / "report.md").read_text()
            self.assertIn("Aggregate", markdown)
            self.assertIn("input_token", markdown)
            self.assertIn("60.00 / 70.00 / +10.00", markdown)
            self.assertIn("366.67 / 320.00 / -12.73%", markdown)
            self.assertIn(
                "calculated directly from the displayed Aggregate values", markdown
            )
            self.assertIn("not an average of task percentages", markdown)
            self.assertLess(
                markdown.index("| **After MAD task screening** |"),
                markdown.index("| reflex:6 |"),
            )
            self.assertNotIn("| cost", markdown.lower())
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
                "invalid JSON"
                if len(requests) == 2
                else json.dumps(
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
            )
            return {"choices": [{"message": {"content": content}}]}

        with patch("zg_bench.engines.judge.time.sleep"):
            result = judge_candidate(
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

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args: Any) -> None:
                pass

            def do_POST(self) -> None:
                length = int(self.headers["Content-Length"])
                requests.append(json.loads(self.rfile.read(length)))
                body = json.dumps(
                    {
                        "id": "local-judge-response",
                        "object": "chat.completion",
                        "created": 1,
                        "model": requests[-1]["model"],
                        "choices": [
                            {
                                "index": 0,
                                "finish_reason": "stop",
                                "message": {"role": "assistant", "content": content},
                            }
                        ],
                        "usage": {
                            "prompt_tokens": 20,
                            "completion_tokens": 10,
                            "total_tokens": 30,
                        },
                    }
                ).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with patch.dict(
                "os.environ",
                {"LITELLM_LOCAL_MODEL_COST_MAP": "True", "DO_NOT_TRACK": "True"},
            ):
                for model in ("glm-5.2", "qwen3.8-max"):
                    with self.subTest(model=model):
                        result = judge_candidate(
                            completion_fn=default_completion(),
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
                self.assertEqual(
                    request["temperature"],
                    OPENCODE_QWEN_TEMPERATURE if model == "qwen3.8-max" else 0,
                )
                self.assertEqual(request["seed"], 42)
                self.assertIs(
                    request["enable_thinking"],
                    OPENCODE_QWEN_ENABLE_THINKING if model == "qwen3.8-max" else True,
                )
                self.assertEqual(
                    request["reasoning_effort"],
                    OPENCODE_QWEN_REASONING_EFFORT
                    if model == "qwen3.8-max"
                    else "high",
                )
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
                    "choices": [
                        {
                            "message": {
                                "content": json.dumps(
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
                            }
                        }
                    ],
                    "usage": {"prompt_tokens": 20, "completion_tokens": 10},
                }

            with (
                patch.dict(
                    "os.environ",
                    {
                        "OPENAI_API_KEY": "shared-key",
                        "OPENAI_BASE_URL": "https://shared.invalid/v1",
                        "GLM_API_KEY": "unused-legacy-key",
                        "GLM_BASE_URL": "https://legacy.invalid/v1",
                    },
                    clear=True,
                ),
                patch(
                    "zg_bench.swe_qa.judge.default_completion", return_value=completion
                ),
                patch("builtins.print"),
            ):
                status = swe_qa_main(
                    [
                        "judge",
                        "--pairs-root",
                        str(pairs_root),
                        "--references",
                        str(references),
                        "--output-dir",
                        str(root / "report"),
                        "--expected",
                        "reflex-6",
                        "--model",
                        "qwen3.8-max",
                        "--attempts",
                        "1",
                    ]
                )
            self.assertEqual(status, 0)
            self.assertEqual(len(requests), 6)
            for request in requests:
                self.assertEqual(request["model"], "openai/qwen3.8-max")
                self.assertEqual(request["api_key"], "unused-legacy-key")
                self.assertEqual(request["api_base"], "https://legacy.invalid/v1")
                self.assertEqual(request["temperature"], OPENCODE_QWEN_TEMPERATURE)
                self.assertEqual(request["seed"], 42)
                self.assertEqual(
                    request["extra_body"],
                    {"enable_thinking": OPENCODE_QWEN_ENABLE_THINKING},
                )
                self.assertEqual(
                    request["reasoning_effort"], OPENCODE_QWEN_REASONING_EFFORT
                )
                self.assertNotIn("response_format", request)
            report = aggregate_reports(
                reports_root=root / "report", output_dir=root / "combined"
            )
            self.assertEqual(report["judge"]["model"], "qwen3.8-max")
            self.assertEqual(report["judge"]["label"], "qwen3.8-max-self-judge-v1")
            self.assertEqual(report["judge"]["temperature"], OPENCODE_QWEN_TEMPERATURE)
            self.assertEqual(
                report["judge"]["enable_thinking"], OPENCODE_QWEN_ENABLE_THINKING
            )
            self.assertEqual(report["gate"]["successful_judgements"], 6)
            for profile in report["cases"][0]["profiles"].values():
                for result in (
                    profile["judge"],
                    *(trial["judge"] for trial in profile["trials"]),
                ):
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
                pairs_root=Path("missing"),
                references_path=Path("missing"),
                output_dir=Path("missing"),
                expected=["reflex:6"],
                model="unknown",
            )

    def test_qwen_judge_shared_credentials_fallback_strips_legacy_key(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            pairs_root, references = self._write_pair_and_reference(root)
            requests: list[dict[str, Any]] = []

            def completion(**kwargs: Any) -> dict[str, Any]:
                requests.append(kwargs)
                return {
                    "choices": [
                        {
                            "message": {
                                "content": json.dumps(
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
                            }
                        }
                    ]
                }

            with patch.dict(
                "os.environ",
                {
                    "GLM_API_KEY": "  ",
                    "OPENAI_API_KEY": " shared-key ",
                    "OPENAI_BASE_URL": "https://shared.invalid/v1",
                },
                clear=True,
            ):
                judge_pairs(
                    pairs_root=pairs_root,
                    references_path=references,
                    output_dir=root / "report",
                    expected=["reflex-6"],
                    completion_fn=completion,
                    model="qwen3.8-max",
                    attempts=1,
                )
            self.assertEqual(len(requests), 6)
            self.assertTrue(
                all(request["api_key"] == "shared-key" for request in requests)
            )
            self.assertTrue(
                all(
                    request["api_base"] == "https://shared.invalid/v1"
                    for request in requests
                )
            )

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
                            f"must be an integer between 1 and {MAX_JUDGE_CONCURRENCY}",
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

    def test_high_input_task_remains_in_offline_aggregation(
        self,
    ) -> None:
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
                content = json.dumps(
                    {
                        key: 18
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
                    "usage": {"prompt_tokens": 10, "completion_tokens": 5},
                }

            with patch.dict("os.environ", {"GLM_API_KEY": "mock"}):
                source = judge_pairs(
                    pairs_root=pairs_root,
                    references_path=references,
                    output_dir=root / "source",
                    expected=["reflex-6"],
                    completion_fn=fake_completion,
                )
            self.assertEqual(len(calls), 6)
            combined = aggregate_reports(
                reports_root=root / "source",
                output_dir=root / "combined",
                expected=["reflex:6"],
            )
            for report in (source, combined):
                self.assertTrue(report["gate"]["passed"])
                self.assertEqual(report["gate"]["valid_pairs"], 1)
                self.assertEqual(report["gate"]["successful_judgements"], 6)
                self.assertEqual(report["judge"]["usage"]["calls"], 6)
                self.assertEqual(len(report["cases"]), 1)
                aggregate = report["aggregate"]
                self.assertEqual(aggregate["filter"]["task_count"], 1)
                self.assertEqual(
                    aggregate["filter"]["profiles"]["zvec-grep"]["excluded_count"], 2
                )
                self.assertEqual(
                    aggregate["profiles"]["zvec-grep"]["input_tokens"], 10000
                )
            for directory in ("source", "combined"):
                markdown = (root / directory / "report.md").read_text()
                self.assertIn("| reflex:6 |", markdown)
                self.assertIn("Excluded trials (4)", markdown)

    def test_large_score_difference_keeps_task_and_can_merge(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            pairs_root, references = self._write_pair_and_reference(root)
            calls: list[dict[str, Any]] = []

            def fake_completion(**kwargs: Any) -> dict[str, Any]:
                calls.append(kwargs)
                score = (
                    15
                    if "Candidate answer:\nzvec-grep"
                    in kwargs["messages"][0]["content"]
                    else 10
                )
                content = json.dumps(
                    {
                        key: score
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
                    "usage": {"prompt_tokens": 10, "completion_tokens": 5},
                }

            with patch.dict("os.environ", {"GLM_API_KEY": "mock"}):
                source = judge_pairs(
                    pairs_root=pairs_root,
                    references_path=references,
                    output_dir=root / "source",
                    expected=["reflex-6"],
                    completion_fn=fake_completion,
                )
            combined = aggregate_reports(
                reports_root=root / "source",
                output_dir=root / "combined",
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
                self.assertEqual(aggregate["filter"]["task_count"], 1)
                self.assertEqual(aggregate["comparison"]["judge_delta"], 25)
                self.assertEqual(
                    aggregate["filter"]["profiles"]["zvec-grep"]["excluded_count"], 2
                )
            for directory in ("source", "combined"):
                markdown = (root / directory / "report.md").read_text()
                self.assertIn("| reflex:6 | 50.00 / 75.00 / +25.00", markdown)
                self.assertIn("Excluded trials (4)", markdown)

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


if __name__ == "__main__":
    unittest.main()
