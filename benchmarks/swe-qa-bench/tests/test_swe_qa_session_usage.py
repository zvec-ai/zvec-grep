"""Recursive session usage collection and reporting contracts."""

from __future__ import annotations

import copy
import json
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

from swe_qa_fixtures import (
    _harbor_trial,
    _judged_task_report,
    _session_usage_fixture,
    _write_harbor_job,
    _write_json,
)
from swe_qa_fixtures import (
    setUpModule as setUpModule,
)
from swe_qa_fixtures import (
    tearDownModule as tearDownModule,
)
from zg_bench.metrics.usage import SESSION_USAGE_METRICS, SESSION_USAGE_SCOPE
from zg_bench.reports.aggregate import aggregate_reports
from zg_bench.swe_qa import SweQaError
from zg_bench.swe_qa.collect import collect_pair
from zg_bench.swe_qa.judge import judge_pairs


class SessionUsageReportingTests(unittest.TestCase):
    def _jobs(self, root: Path) -> list[Path]:
        trial_dirs = []
        for profile in ("baseline", "zvec-grep"):
            usage = _session_usage_fixture()
            _write_harbor_job(
                root,
                profile=profile,
                trials=[
                    _harbor_trial(
                        f"reflex-6-{profile}",
                        answer=f"root final {profile}",
                        input_tokens=600,
                        output_tokens=120,
                        tool_calls=1,
                        agent_wall_seconds=22,
                        cost_usd=usage["total"]["cost_usd"],
                    )
                ],
            )
            trial_dir = root / f"fixture-reflex-6-{profile}" / f"reflex-6-{profile}"
            result = json.loads((trial_dir / "result.json").read_text())
            result["config"] = {"agent": {"kwargs": {"collect_session_usage": True}}}
            result["agent_result"]["n_cache_tokens"] = 335
            _write_json(trial_dir / "result.json", result)
            _write_json(trial_dir / "agent" / "session-usage.json", usage)
            trial_dirs.append(trial_dir)
        return trial_dirs

    def test_collects_nested_usage_and_preserves_root_answer_and_wall_time(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            self._jobs(root)
            pair = collect_pair(
                runs_dir=root, task="reflex:6", output=root / "pair.json"
            )
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
            with (
                self.subTest(failure=failure),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
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
                    collect_pair(
                        runs_dir=root, task="reflex:6", output=root / "pair.json"
                    )
                self.assertFalse((root / "pair.json").exists())

    def test_rejects_inconsistent_totals_context_and_collection_time(self) -> None:
        for failure in (
            "total",
            "input_components",
            "output_components",
            "context",
            "cache_context",
            "wall",
        ):
            with (
                self.subTest(failure=failure),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
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
                    collect_pair(
                        runs_dir=root, task="reflex:6", output=root / "pair.json"
                    )

    def test_unknown_cost_propagates_with_or_without_children(self) -> None:
        for children in (True, False):
            with (
                self.subTest(children=children),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
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
                        n_input_tokens=total["input_tokens"],
                        n_output_tokens=total["output_tokens"],
                        n_cache_tokens=total["cache_read_tokens"]
                        + total["cache_write_tokens"],
                        cost_usd=None,
                    )
                    _write_json(path, usage)
                    _write_json(trial_dir / "result.json", result)
                pair = collect_pair(
                    runs_dir=root, task="reflex:6", output=root / "pair.json"
                )
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
            _write_json(
                references,
                {
                    "references": [
                        {
                            "task_id": "reflex:6",
                            "question": "question",
                            "reference_answer": "reference",
                        }
                    ]
                },
            )
            prompts: list[str] = []

            def completion(**kwargs: Any) -> dict[str, Any]:
                prompts.append(kwargs["messages"][0]["content"])
                return {
                    "choices": [
                        {
                            "message": {
                                "content": json.dumps(
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
                            }
                        }
                    ],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 10},
                }

            with patch.dict("os.environ", {"GLM_API_KEY": "mock"}):
                report = judge_pairs(
                    pairs_root=pair_path,
                    references_path=references,
                    output_dir=root / "report",
                    expected=["reflex:6"],
                    model="glm-5.2",
                    completion_fn=completion,
                )
            self.assertEqual(report["usage_scope"], SESSION_USAGE_SCOPE)
            self.assertEqual(len(prompts), 2)
            self.assertTrue(
                all("Candidate answer:\nroot final " in prompt for prompt in prompts)
            )
            for profile in report["aggregate"]["profiles"].values():
                self.assertEqual(profile["input_tokens"], 600)
                self.assertEqual(profile["tool_calls"], 8)
                self.assertEqual(profile["agent_wall_seconds"], 20)
                self.assertEqual(
                    profile["session_usage"]["descendants"]["input_tokens"], 500
                )
            markdown = (root / "report" / "report.md").read_text()
            self.assertNotIn("Text output / reasoning", markdown)
            self.assertNotIn("Cache read / write", markdown)
            self.assertNotIn("| descendants |", markdown)
            self.assertNotIn("Root/subagent", markdown)
            self.assertIn("not a complete provider bill", markdown)
            aggregated = aggregate_reports(
                reports_root=root / "report", output_dir=root / "aggregate"
            )
            self.assertEqual(aggregated["usage_scope"], SESSION_USAGE_SCOPE)

    def _new_report(self, task_id: str) -> dict[str, Any]:
        report = _judged_task_report(task_id)
        report["usage_scope"] = SESSION_USAGE_SCOPE
        usage = _session_usage_fixture()
        for profile in report["cases"][0]["profiles"].values():
            for metrics in [
                profile["metrics"],
                *[trial["metrics"] for trial in profile["trials"]],
            ]:
                metrics.update(copy.deepcopy(usage["total"]))
                metrics["usage_scope"] = SESSION_USAGE_SCOPE
                metrics["session_usage"] = copy.deepcopy(usage)
        return report

    def test_aggregate_rejects_legacy_without_scope_mixed_with_new_reports(
        self,
    ) -> None:
        for new_first in (True, False):
            with (
                self.subTest(new_first=new_first),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                new = self._new_report("reflex:6")
                legacy = _judged_task_report("requests:16")
                self.assertNotIn("usage_scope", legacy)
                for index, report in enumerate(
                    (new, legacy) if new_first else (legacy, new)
                ):
                    _write_json(root / str(index) / "report.json", report)
                with self.assertRaisesRegex(SweQaError, "usage scopes"):
                    aggregate_reports(reports_root=root, output_dir=root / "aggregate")

    def test_report_rejects_inconsistent_summary_or_mixed_trial_scope(self) -> None:
        for failure in ("mean", "trial_scope"):
            with (
                self.subTest(failure=failure),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
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


if __name__ == "__main__":
    unittest.main()
