"""Offline regressions for report reuse after rerunning failed matrix jobs."""

from __future__ import annotations

import copy
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from swe_qa_fixtures import (
    EXPECTED_TASK_IDS,
    _judged_task_report,
    _set_report_trial_judgements,
    _write_json,
)
from zg_bench.ci.task_reports import (
    MANIFEST,
    run_identity,
    select_reports,
    stamp_report,
)
from zg_bench.core.errors import SweQaError
from zg_bench.reports.aggregate import aggregate_reports


class TaskReportAttemptTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = {
            "GITHUB_RUN_ID": "123",
            "GITHUB_RUN_ATTEMPT": "2",
            "BENCH_CANDIDATE_COMMIT": "a" * 40,
            "BENCH_CANDIDATE_REPOSITORY": "Cuiyus/zvec-grep",
            "BENCH_HARNESS_COMMIT": "b" * 40,
            "BENCH_PROTOCOL_HASH": "c" * 64,
            "BENCH_SCOPE": "all-full",
            "SWE_QA_MODEL": "glm-5.2",
            "BENCH_EMBEDDING_RUNTIME": "local",
            "BENCH_EMBEDDING_MODEL": "local/potion-code-16m-v2",
        }
        self.identity = run_identity(self.env)

    def report(self, task: str, attempt: int, *, score: int = 80) -> Path:
        directory = self.root / "reports" / f"{task}-123-{attempt}"
        source = _judged_task_report(task)
        _set_report_trial_judgements(source, baseline=[75] * 5, zvec=[score] * 5)
        _write_json(directory / "report.json", source)
        stamp_report(directory, {**self.identity, "run_attempt": attempt})
        return directory

    def select(self, expected=EXPECTED_TASK_IDS):
        return select_reports(
            reports_root=self.root / "reports",
            output_dir=self.root / "selected",
            identity=self.identity,
            expected=expected,
        )

    def test_nineteen_successes_plus_one_rerun_produce_complete_twenty_task_report(
        self,
    ):
        for task in EXPECTED_TASK_IDS[:-1]:
            self.report(task, 1)
        self.report(EXPECTED_TASK_IDS[-1], 2)
        selection = self.select()
        result = aggregate_reports(
            reports_root=self.root / "selected",
            output_dir=self.root / "aggregate",
            expected=EXPECTED_TASK_IDS,
        )
        self.assertTrue(result["gate"]["passed"])
        self.assertEqual(result["gate"]["missing_tasks"], [])
        self.assertEqual(result["gate"]["valid_pairs"], 20)
        self.assertEqual(result["gate"]["successful_judgements"], 200)
        self.assertEqual(
            [row["attempt"] for row in selection["selected"]], [1] * 19 + [2]
        )

    def test_latest_valid_report_replaces_previous_task_without_double_counting(self):
        task = EXPECTED_TASK_IDS[0]
        self.report(task, 1, score=80)
        self.report(task, 2, score=90)
        selection = self.select([task])
        result = aggregate_reports(
            reports_root=self.root / "selected",
            output_dir=self.root / "aggregate",
            expected=[task],
        )
        self.assertEqual(selection["selected"][0]["attempt"], 2)
        self.assertEqual(
            result["cases"][0]["profiles"]["zvec-grep"]["judge"]["total"], 90
        )
        self.assertEqual(result["gate"]["successful_judgements"], 10)

    def test_invalid_latest_report_keeps_earlier_valid_result(self):
        task = EXPECTED_TASK_IDS[0]
        self.report(task, 1)
        latest = self.report(task, 2)
        (latest / "report.json").write_text("{}")
        result = self.select([task])
        self.assertEqual(result["selected"][0]["attempt"], 1)
        self.assertIn("checksum mismatch", result["ignored"][0]["reason"])

    def test_other_candidates_runs_or_configurations_are_not_reused(self):
        task = EXPECTED_TASK_IDS[0]
        changes = {
            "candidate_commit": "d" * 40,
            "candidate_repository": "other/repo",
            "run_id": "124",
            "harness_commit": "e" * 40,
            "protocol_sha256": "f" * 64,
            "configuration": {**self.identity["configuration"], "model": "qwen3.8-max"},
            "run_attempt": 3,
        }
        for key, value in changes.items():
            with self.subTest(key=key), tempfile.TemporaryDirectory() as output:
                directory = self.report(task, 1)
                manifest = json.loads((directory / MANIFEST).read_text())
                manifest[key] = value
                _write_json(directory / MANIFEST, manifest)
                with self.assertRaisesRegex(SweQaError, "no valid task reports"):
                    select_reports(
                        reports_root=self.root / "reports",
                        output_dir=Path(output) / "selected",
                        identity=self.identity,
                        expected=[task],
                    )

    def test_missing_task_remains_incomplete_and_is_not_filled_with_zero(self):
        self.report(EXPECTED_TASK_IDS[0], 1)
        selection = self.select(EXPECTED_TASK_IDS[:2])
        result = aggregate_reports(
            reports_root=self.root / "selected",
            output_dir=self.root / "aggregate",
            expected=EXPECTED_TASK_IDS[:2],
            allow_missing=True,
        )
        self.assertEqual(selection["missing_tasks"], [EXPECTED_TASK_IDS[1]])
        self.assertFalse(result["gate"]["passed"])
        self.assertEqual(len(result["cases"]), 1)

    def test_unstamped_old_reports_cannot_be_reused(self):
        directory = self.report(EXPECTED_TASK_IDS[0], 1)
        (directory / MANIFEST).unlink()
        with self.assertRaisesRegex(SweQaError, "no valid task reports"):
            self.select()

    def test_duplicate_task_in_same_attempt_is_rejected(self):
        directory = self.report(EXPECTED_TASK_IDS[0], 1)
        duplicate = self.root / "reports" / "duplicate"
        duplicate.mkdir()
        for name in ("report.json", MANIFEST):
            (duplicate / name).write_bytes((directory / name).read_bytes())
        with self.assertRaisesRegex(SweQaError, "duplicate task report"):
            self.select()

    def test_single_artifact_flat_download_layout_and_cli(self):
        task = EXPECTED_TASK_IDS[0]
        directory = self.report(task, 2)
        import os

        result = subprocess.run(
            [
                sys.executable,
                "-m",
                "zg_bench.ci.task_reports",
                "select",
                "--reports-root",
                str(directory),
                "--output-dir",
                str(self.root / "selected"),
                "--expected",
                task,
            ],
            env={**os.environ, **self.env},
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["selected"][0]["attempt"], 2)

    def test_endpoint_and_embedding_config_changes_invalidate_reuse_without_storing_keys(
        self,
    ):
        changed = run_identity(
            {**self.env, "SWE_QA_MODEL_BASE_URL": "https://other.example/v1"}
        )
        self.assertNotEqual(changed["configuration"], self.identity["configuration"])
        remote_env = {
            **self.env,
            "BENCH_EMBEDDING_RUNTIME": "remote",
            "BENCH_EMBEDDING_MODEL": "qwen/qwen3.7-text-embedding",
            "BENCH_EMBEDDING_ENDPOINT": "https://embedding.example/v1",
            "GLM_API_KEY": "not-for-artifacts",
            "ZVEC_GREP_API_KEY": "also-secret",
        }
        self.assertNotIn("not-for-artifacts", json.dumps(run_identity(remote_env)))
        self.assertNotIn("also-secret", json.dumps(run_identity(remote_env)))
        self.assertNotEqual(
            run_identity(remote_env)["configuration"], self.identity["configuration"]
        )

    def test_stamp_rejects_wrong_model_or_trial_count(self):
        directory = self.report(EXPECTED_TASK_IDS[0], 1)
        wrong_model = copy.deepcopy(self.identity)
        wrong_model["configuration"]["model"] = "qwen3.8-max"
        with self.assertRaisesRegex(SweQaError, "different judge model"):
            stamp_report(directory, wrong_model)
        wrong_trials = copy.deepcopy(self.identity)
        wrong_trials["configuration"]["trials_per_profile"] = 3
        with self.assertRaisesRegex(SweQaError, "different trial count"):
            stamp_report(directory, wrong_trials)
