from __future__ import annotations

import hashlib
import io
import json
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

from zg_bench.artifacts import fingerprint, read_json, sha256_file, write_json
from zg_bench.config import load_config
from zg_bench.console import Console
from zg_bench.evaluate import (
    EVALUATION_SOURCE_SHA256,
    EVALUATOR_TIMEOUT_SECONDS,
    JUDGE_PROMPT,
    JUDGE_PROMPT_SHA256,
    JUDGE_SCHEMA,
    JUDGE_SCHEMA_SHA256,
    USAGE_AUDIT_SCHEMA,
    _completed_judgement,
    _evaluation_cost,
    _parse_judgement,
    _run_judge,
    _run_usage_audit,
    _usage_audit_current,
    evaluate_run,
)
from zg_bench.models import ToolCall, TraceSummary, Usage
from zg_bench.process import CommandResult


class OpenCodeEvaluatorTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.artifacts = self.root / "artifacts"
        self.run_root = self.artifacts / "runs" / "run-1"
        self.home = self.run_root / "profiles" / "judge" / "opencode-home"
        self.workspace = self.root / "empty-workspace"
        self.workspace.mkdir()
        self.opencode = self.root / "opencode"
        self.opencode.write_text("test executable")
        self.schema = self.run_root / "evaluation" / "judge-schema.json"
        write_json(self.schema, JUDGE_SCHEMA)
        self.official = (
            self.run_root
            / "evaluation"
            / "input"
            / "baseline"
            / "770"
            / "trial-001.json"
        )
        write_json(self.official, {"result": [{"output": "Paris"}]})
        self.case_result = (
            self.run_root
            / "cases"
            / "770"
            / "baseline"
            / "trials"
            / "trial-001"
            / "result.json"
        )
        write_json(self.case_result, {"trace": {"final_response": "Paris"}})
        self.console = Console(stream=io.StringIO())
        self.judgement = {
            "extracted_final_answer": "Paris",
            "correct": True,
            "reasoning": "Equivalent to the reference answer.",
            "confidence": 90,
        }
        self.trace = TraceSummary(
            thread_id="session-1",
            final_response=json.dumps(self.judgement),
            last_agent_message=json.dumps(self.judgement),
            turn_completed=True,
            usage=Usage(input_tokens=125, cached_input_tokens=25, output_tokens=20),
            tool_calls=(),
            observed_docids=(),
        )
        self.outcome = {
            "trace": self.trace,
            "exit_code": 0,
            "idle_killed": False,
            "wall_seconds": 2.5,
        }

    def judge(self):
        return _run_judge(
            run_root=self.run_root,
            opencode=self.opencode,
            opencode_home=self.home,
            workspace=self.workspace,
            schema_path=self.schema,
            run_id="run-1",
            query_id="770",
            trial_index=1,
            profile="baseline",
            official_input=self.official,
            question="Which city?",
            correct_answer="Paris",
            model="custom-openai/qwen3.8-max",
            reasoning_effort="high",
            position=1,
            total=1,
            console=self.console,
        )

    def completed(self):
        return _completed_judgement(
            self.run_root
            / "evaluation"
            / "results"
            / "baseline"
            / "770"
            / "trial-001.json",
            run_root=self.run_root,
            profile="baseline",
            query_id="770",
            trial_index=1,
        )

    @patch("zg_bench.evaluate.run_prompt")
    def test_judge_uses_isolated_home_explicit_schema_and_preserves_usage(
        self, execute
    ):
        execute.return_value = self.outcome
        result = self.judge()
        self.assertEqual(result["status"], "completed")
        self.assertTrue(result["correct"])
        self.assertEqual(result["usage"]["input_tokens"], 125)
        self.assertEqual(result["usage"]["cached_input_tokens"], 25)
        self.assertEqual(result["usage"]["output_tokens"], 20)
        self.assertEqual(result["wall_seconds"], 2.5)
        self.assertEqual(result["judge_prompt_sha256"], JUDGE_PROMPT_SHA256)
        self.assertEqual(result["judge_schema_sha256"], JUDGE_SCHEMA_SHA256)
        self.assertEqual(result["evaluation_source_sha256"], EVALUATION_SOURCE_SHA256)
        self.assertEqual(
            result["candidate_sha256"], hashlib.sha256(b"Paris").hexdigest()
        )
        self.assertEqual(execute.call_args.args, (self.opencode,))
        options = execute.call_args.kwargs
        self.assertEqual(options["home"], self.home)
        self.assertEqual(options["workspace"], self.workspace)
        self.assertEqual(options["model"], "custom-openai/qwen3.8-max")
        self.assertEqual(options["timeout_seconds"], EVALUATOR_TIMEOUT_SECONDS)
        self.assertEqual(EVALUATOR_TIMEOUT_SECONDS, 900)
        prompt, serialized_schema = options["prompt"].split("\n\nOUTPUT_SCHEMA:\n")
        self.assertTrue(prompt.startswith(JUDGE_PROMPT.split("INPUT_DATA:")[0]))
        self.assertEqual(json.loads(serialized_schema), JUDGE_SCHEMA)
        self.assertEqual(Path(result["paths"]["prompt"]).read_text(), options["prompt"])
        self.assertTrue(result["paths"]["response"].endswith("response.md"))
        self.assertTrue(self.completed())

    @patch("zg_bench.evaluate.run_prompt")
    def test_judge_rejects_any_tool_use_and_keeps_billed_usage(self, execute):
        execute.return_value = {
            **self.outcome,
            "trace": replace(
                self.trace, tool_calls=(ToolCall(kind="tool", name="read"),)
            ),
        }
        result = self.judge()
        self.assertEqual(result["status"], "failed")
        self.assertIn("no-tools", result["error"])
        self.assertEqual(result["usage"]["input_tokens"], 125)
        self.assertFalse(self.completed())

    @patch("zg_bench.evaluate.run_prompt")
    def test_process_errors_incomplete_turn_and_idle_timeout_cannot_score(
        self, execute
    ):
        for changes, message in (
            ({"exit_code": 1}, "did not complete"),
            ({"trace": replace(self.trace, turn_completed=False)}, "did not complete"),
            (
                {"trace": replace(self.trace, errors=("provider unavailable",))},
                "provider unavailable",
            ),
            ({"idle_killed": True}, "time limit"),
        ):
            with self.subTest(changes=changes):
                execute.return_value = {**self.outcome, **changes}
                result = self.judge()
                self.assertEqual(result["status"], "failed")
                self.assertIn(message, result["error"])
                self.assertFalse(self.completed())

    @patch("zg_bench.evaluate.run_prompt")
    def test_invalid_schema_is_a_resumable_failure(self, execute):
        execute.return_value = {
            **self.outcome,
            "trace": replace(self.trace, final_response='{"correct": true}'),
        }
        failed = self.judge()
        self.assertEqual(failed["status"], "failed")
        self.assertEqual(failed["attempt"], 1)
        self.assertFalse(self.completed())
        execute.return_value = self.outcome
        completed = self.judge()
        self.assertEqual(completed["status"], "completed")
        self.assertEqual(completed["attempt"], 2)
        self.assertTrue(self.completed())

    @patch("zg_bench.evaluate.run_prompt")
    def test_launch_failure_is_written_as_failed_judgement(self, execute):
        execute.side_effect = OSError("executable cannot be started")
        result = self.judge()
        self.assertEqual(result["status"], "failed")
        self.assertIn("cannot be started", result["error"])
        self.assertIsNone(result["usage"])
        self.assertTrue(Path(result["paths"]["stderr"]).is_file())
        self.assertFalse(self.completed())

    def test_strict_json_schema_rejects_wrong_types_extra_keys_and_non_json(self):
        for response in (
            "[]",
            "null",
            "```json\n" + json.dumps(self.judgement) + "\n```",
            json.dumps({**self.judgement, "correct": "true"}),
            json.dumps({**self.judgement, "confidence": True}),
            json.dumps({**self.judgement, "confidence": 101}),
            json.dumps({**self.judgement, "confidence": 10**400}),
            json.dumps({**self.judgement, "confidence": float("nan")}),
            json.dumps({**self.judgement, "confidence": float("inf")}),
            json.dumps({**self.judgement, "extra": "forbidden"}),
            json.dumps(self.judgement).replace(
                '"correct": true', '"correct": true, "correct": false'
            ),
        ):
            with (
                self.subTest(response=response),
                self.assertRaises((TypeError, ValueError)),
            ):
                _parse_judgement(response, JUDGE_SCHEMA)
        self.assertEqual(
            _parse_judgement(json.dumps(self.judgement), JUDGE_SCHEMA), self.judgement
        )

    @patch("zg_bench.evaluate.run_prompt")
    def test_usage_audit_keeps_case_hash_and_schema_and_rejects_judge_tools(
        self, execute
    ):
        case_path = (
            self.run_root
            / "cases"
            / "770"
            / "zvec-grep"
            / "trials"
            / "trial-001"
            / "result.json"
        )
        write_json(
            case_path,
            {
                "trace": {
                    "final_response": "Paris",
                    "tool_calls": [
                        {
                            "name": "zvec_grep_search",
                            "output": "Paris",
                            "status": "completed",
                        }
                    ],
                }
            },
        )
        schema = self.run_root / "evaluation" / "usage-audit-schema.json"
        write_json(schema, USAGE_AUDIT_SCHEMA)
        audit_trace = replace(
            self.trace,
            final_response=json.dumps(
                {
                    "used_zvec_grep": True,
                    "correct_usage": True,
                    "reasoning": "Relevant evidence.",
                }
            ),
        )
        for tools in ((), (ToolCall(kind="tool", name="bash"),)):
            execute.return_value = {
                **self.outcome,
                "trace": replace(audit_trace, tool_calls=tools),
            }
            result = _run_usage_audit(
                run_root=self.run_root,
                opencode=self.opencode,
                opencode_home=self.home,
                workspace=self.workspace,
                schema_path=schema,
                query_id="770",
                trial_index=1,
                question="Which city?",
                model="custom-openai/qwen3.8-max",
                reasoning_effort="high",
                position=1,
                total=1,
                console=self.console,
            )
            self.assertEqual(result["status"], "failed" if tools else "completed")
            self.assertEqual(result["case_result_sha256"], sha256_file(case_path))
            self.assertEqual(result["observed_zvec_grep_calls"], 1)
            self.assertEqual(result["usage"]["input_tokens"], 125)
            self.assertEqual(
                _usage_audit_current(
                    self.run_root
                    / "evaluation"
                    / "usage-audit"
                    / "results"
                    / "770"
                    / "trial-001.json",
                    run_root=self.run_root,
                    query_id="770",
                    trial_index=1,
                ),
                not tools,
            )
            self.assertEqual(
                json.loads(
                    execute.call_args.kwargs["prompt"].split("\n\nOUTPUT_SCHEMA:\n")[1]
                ),
                USAGE_AUDIT_SCHEMA,
            )

    def test_evaluation_token_cost_retains_unavailable_usage_and_wall_time(self):
        result = _evaluation_cost(
            [
                {"usage": self.trace.usage.to_dict(), "wall_seconds": 2.5},
                {"usage": None, "wall_seconds": 1.5},
            ]
        )
        self.assertEqual(
            result,
            {
                "completed_calls": 2,
                "usage_available": 1,
                "usage_unavailable": 1,
                "input_tokens": 125,
                "cached_input_tokens": 25,
                "output_tokens": 20,
                "reasoning_output_tokens": 0,
                "wall_seconds": 4.0,
            },
        )

    def test_evaluate_verifies_version_configuration_and_endpoint_before_judge_setup(
        self,
    ):
        query = {"query_id": "770", "query": "Which city?", "answer": "Paris"}
        config = load_config()
        metadata = {
            "run_id": "run-1",
            "query_ids": ["770"],
            "suite": "ci",
            "model": "custom-openai/qwen3.8-max",
            "reasoning_effort": "high",
            "protocol": {
                "configuration_sha256": sha256_file(config.path),
                "opencode_sha256": sha256_file(self.opencode),
                "opencode_version": "opencode 1.0.0",
                "model_base_url": "https://provider.example/v1",
                "selected_queries_sha256": fingerprint(
                    [json.dumps(query, sort_keys=True, ensure_ascii=False)]
                ),
            },
        }
        write_json(self.run_root / "run.json", metadata)
        with (
            patch("zg_bench.evaluate._eligible_trial_pairs", return_value=[("770", 1)]),
            patch("zg_bench.evaluate.resolve_executable", return_value=self.opencode),
            patch(
                "zg_bench.evaluate.run_command",
                return_value=CommandResult((), 0, "opencode 1.0.0", ""),
            ),
            patch("zg_bench.evaluate.load_queries", return_value=[query]),
            patch(
                "zg_bench.evaluate.model_base_url",
                return_value="https://provider.example/v1",
            ),
            patch("zg_bench.evaluate.evaluation_complete", return_value=False),
            patch(
                "zg_bench.evaluate.prepare_judge_home", return_value=self.home
            ) as prepare_home,
            patch(
                "zg_bench.evaluate.export_official",
                return_value=self.official.parents[2],
            ),
            patch(
                "zg_bench.evaluate._judge_items",
                return_value=[("770", 1, "baseline", self.official)],
            ),
            patch("zg_bench.evaluate._run_judge") as judge,
            patch(
                "zg_bench.evaluate.summarize_evaluation",
                return_value=self.root / "report.md",
            ) as summarize,
        ):
            evaluate_run(
                self.artifacts, self.run_root, config=config, console=self.console
            )
            metadata["protocol"]["model_base_url"] = "https://other.example/v1"
            write_json(self.run_root / "run.json", metadata)
            with self.assertRaisesRegex(RuntimeError, "endpoint changed"):
                evaluate_run(
                    self.artifacts, self.run_root, config=config, console=self.console
                )
            metadata["protocol"]["model_base_url"] = "https://provider.example/v1"
            metadata["protocol"]["configuration_sha256"] = "different-config"
            write_json(self.run_root / "run.json", metadata)
            with self.assertRaisesRegex(RuntimeError, "configuration changed"):
                evaluate_run(
                    self.artifacts, self.run_root, config=config, console=self.console
                )
            metadata["protocol"]["opencode_version"] = "different-version"
            write_json(self.run_root / "run.json", metadata)
            with self.assertRaisesRegex(RuntimeError, "version changed"):
                evaluate_run(
                    self.artifacts, self.run_root, config=config, console=self.console
                )
        prepare_home.assert_called_once_with(config, self.artifacts, self.home)
        self.assertEqual(judge.call_args.kwargs["opencode_home"], self.home)
        summarize.assert_called_once_with(
            self.run_root, opencode_version="opencode 1.0.0"
        )
        self.assertEqual(read_json(self.schema), JUDGE_SCHEMA)


if __name__ == "__main__":
    unittest.main()
