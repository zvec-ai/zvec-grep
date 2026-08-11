from __future__ import annotations

import json
import shutil
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

from zg_bench.swe_qa import SELF_JUDGE_LABEL, SweQaError
from zg_bench.swe_qa.collect import collect_pair
from zg_bench.swe_qa.judge import judge_pairs
from zg_bench.swe_qa.validation import validate_assets

CODING_DIR = Path(__file__).resolve().parents[1]
SELECTION_PATH = CODING_DIR / "zg_bench" / "swe_qa" / "data" / "selection.json"
REFERENCES_PATH = CODING_DIR / "zg_bench" / "swe_qa" / "data" / "references.json"
DATASET_PATH = CODING_DIR / "datasets" / "swe-qa-bench-manual"


def _write_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value), encoding="utf-8")


def _write_harbor_job(
    root: Path,
    *,
    profile: str,
    answer: str,
    input_tokens: int,
    output_tokens: int,
    tool_calls: int,
    cost_usd: float | None,
) -> None:
    job_dir = root / f"manual-reflex-6-{profile}"
    trial_dir = job_dir / f"reflex-6-opencode-{profile}"
    _write_json(
        job_dir / "result.json",
        {
            "finished_at": "2026-08-11T10:00:10+00:00",
            "n_total_trials": 1,
            "stats": {
                "n_completed_trials": 1,
                "n_errored_trials": 0,
            },
        },
    )
    _write_json(
        trial_dir / "result.json",
        {
            "task_name": "reflex-6",
            "task_id": {"path": "/dataset/reflex-6"},
            "trial_name": trial_dir.name,
            "finished_at": "2026-08-11T10:00:10+00:00",
            "exception_info": None,
            "agent_info": {
                "name": "opencode",
                "model_info": {"name": "custom-openai/glm-5.2"},
            },
            "agent_result": {
                "n_input_tokens": input_tokens,
                "n_output_tokens": output_tokens,
                "cost_usd": cost_usd,
            },
            "verifier_result": {"rewards": {"reward": 1}},
            "agent_execution": {
                "started_at": "2026-08-11T10:00:00+00:00",
                "finished_at": "2026-08-11T10:00:10+00:00",
            },
        },
    )
    calls = [
        {
            "tool_call_id": f"call-{index}",
            "function_name": "bash",
            "arguments": {"command": "true"},
        }
        for index in range(tool_calls)
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
                    "message": answer,
                    "tool_calls": calls,
                },
            ],
        },
    )


class CollectTests(unittest.TestCase):
    def test_collects_metrics_from_two_completed_harbor_jobs(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            _write_harbor_job(
                root,
                profile="baseline",
                answer="baseline answer",
                input_tokens=120,
                output_tokens=20,
                tool_calls=3,
                cost_usd=0.03,
            )
            _write_harbor_job(
                root,
                profile="zvec-grep",
                answer="zvec answer",
                input_tokens=70,
                output_tokens=15,
                tool_calls=1,
                cost_usd=None,
            )
            output = root / "pairs" / "reflex-6" / "pair.json"

            pair = collect_pair(
                runs_dir=root, task="reflex:6", output=output
            )

            self.assertTrue(pair["valid"])
            self.assertEqual(pair["task_id"], "reflex:6")
            self.assertEqual(pair["profiles"]["baseline"]["input_tokens"], 120)
            self.assertEqual(pair["profiles"]["baseline"]["tool_calls"], 3)
            self.assertEqual(
                pair["profiles"]["baseline"]["agent_wall_seconds"], 10.0
            )
            self.assertIsNone(pair["profiles"]["zvec-grep"]["cost_usd"])
            self.assertEqual(json.loads(output.read_text()), pair)

    def test_empty_final_answer_fails_collection(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            for profile in ("baseline", "zvec-grep"):
                _write_harbor_job(
                    root,
                    profile=profile,
                    answer="" if profile == "zvec-grep" else "answer",
                    input_tokens=10,
                    output_tokens=2,
                    tool_calls=0,
                    cost_usd=None,
                )

            with self.assertRaisesRegex(SweQaError, "empty final answer"):
                collect_pair(
                    runs_dir=root,
                    task="reflex:6",
                    output=root / "pair.json",
                )


class JudgeTests(unittest.TestCase):
    def _write_pair_and_reference(self, root: Path) -> tuple[Path, Path]:
        pairs_root = root / "pairs"
        _write_json(
            pairs_root / "pair-reflex-6.json",
            {
                "schema_version": 1,
                "task_id": "reflex-6",
                "valid": True,
                "profiles": {
                    "baseline": {
                        "answer": "baseline candidate",
                        "input_tokens": 100,
                        "output_tokens": 20,
                        "tool_calls": 10,
                        "agent_wall_seconds": 20.0,
                        "cost_usd": 0.2,
                    },
                    "zvec-grep": {
                        "answer": "zvec candidate",
                        "input_tokens": 40,
                        "output_tokens": 10,
                        "tool_calls": 4,
                        "agent_wall_seconds": 8.0,
                        "cost_usd": None,
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
                score = 15 if "Candidate answer:\nzvec candidate" in prompt_text else 10
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

            self.assertEqual(len(requests), 2)
            self.assertTrue(all(call["temperature"] == 0 for call in requests))
            self.assertTrue(all(call["model"] == "openai/glm-5.2" for call in requests))
            self.assertTrue(all(call["api_key"] == "test-secret" for call in requests))
            self.assertEqual(report["judge"]["label"], SELF_JUDGE_LABEL)
            self.assertTrue(report["judge"]["self_judge"])
            self.assertTrue(report["gate"]["report_only"])
            self.assertFalse(report["gate"]["numeric_thresholds"])
            self.assertEqual(report["cases"][0]["task_id"], "reflex:6")
            self.assertEqual(report["cases"][0]["comparison"]["judge_delta"], 25)
            self.assertEqual(
                report["cases"][0]["comparison"]["input_token_reduction_pct"],
                60.0,
            )
            markdown = (output_dir / "report.md").read_text()
            self.assertIn("Aggregate", markdown)
            self.assertIn("input_token", markdown)
            self.assertIn("N/A", markdown)
            self.assertEqual(summary.read_text(), markdown)
            serialized = (output_dir / "report.json").read_text()
            self.assertNotIn("judge-only reference", serialized)
            self.assertNotIn("test-secret", serialized)

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
