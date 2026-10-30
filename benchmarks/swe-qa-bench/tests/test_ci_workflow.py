"""The workflow must wire the selected Rust package into each paired run."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[3]
BENCHMARK = ROOT / "benchmarks/swe-qa-bench"
WORKFLOW = ROOT / ".github/workflows/swe-qa-bench.yml"
OFFLINE_WORKFLOW = ROOT / ".github/workflows/swe-qa-offline.yml"


class BenchmarkWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = yaml.load(WORKFLOW.read_text(), Loader=yaml.BaseLoader)

    def test_selected_rust_package_reaches_each_pair(self) -> None:
        package = self.workflow["jobs"]["package-candidate"]
        pair = self.workflow["jobs"]["run-pair"]
        build = next(
            step for step in package["steps"] if step.get("id") == "rust-candidate"
        )
        self.assertEqual(build["uses"], "./.github/actions/rust-candidate-package")
        self.assertEqual(build["with"]["candidate_ref"], "${{ inputs.candidate_ref }}")
        self.assertEqual(
            package["outputs"]["candidate-commit"],
            "${{ steps.rust-candidate.outputs.commit }}",
        )
        self.assertIn("package-candidate", pair["needs"])
        pair_commands = "\n".join(step.get("run", "") for step in pair["steps"])
        self.assertIn("rust-package-cache.mjs verify", pair_commands)
        self.assertIn("needs.package-candidate.outputs.candidate-commit", pair_commands)
        self.assertIn(
            '--zvec-grep-package "$RUNNER_TEMP/package/candidate.tgz"', pair_commands
        )

    def test_task_scope_and_trial_count_use_versioned_data(self) -> None:
        config = json.loads((BENCHMARK / "ci-config.json").read_text())
        self.assertEqual(config["trials_per_profile"], 10)
        self.assertEqual(config["max_retries"], 2)
        self.assertEqual(
            config["embedding_models"],
            {
                "local": "local/potion-code-16m-v2",
                "remote": "qwen/qwen3.7-text-embedding",
            },
        )
        workflow_text = WORKFLOW.read_text()
        self.assertIn("benchmarks/swe-qa-bench/ci-config.json", workflow_text)
        self.assertNotIn("SWE_QA_TRIALS_PER_PROFILE", workflow_text)
        self.assertNotIn("SWE_QA_MAX_RETRIES", workflow_text)

        embedding = self.workflow["on"]["workflow_dispatch"]["inputs"]["embedding"]
        self.assertEqual(embedding["default"], "local")
        self.assertEqual(set(embedding["options"]), set(config["embedding_models"]))
        validate = self.workflow["jobs"]["validate"]
        self.assertEqual(
            validate["outputs"]["embedding-model"],
            "${{ steps.embedding.outputs.model }}",
        )
        pair_commands = "\n".join(
            step.get("run", "") for step in self.workflow["jobs"]["run-pair"]["steps"]
        )
        self.assertIn('--embedding-model "$EMBEDDING_MODEL"', pair_commands)
        self.assertIn('--embedding-endpoint "$ZVEC_GREP_ENDPOINT"', pair_commands)

        selection_path = BENCHMARK / "zg_bench/swe_qa/data/selection.json"
        selection = json.loads(selection_path.read_text())
        slugs_by_id = {
            task["task_id"]: task["task_slug"] for task in selection["tasks"]
        }
        all_ids = list(slugs_by_id)
        scopes = {
            "repro-3": ["reflex:6", "requests:16", "conan:39"],
            "smoke": selection["gate"]["auto_tasks"],
            "all-full": all_ids,
            "gate-20": all_ids,
        }
        for scope, expected_ids in scopes.items():
            with self.subTest(scope=scope):
                result = subprocess.run(
                    [
                        sys.executable,
                        "-m",
                        "zg_bench.swe_qa",
                        "matrix",
                        "--selection",
                        str(selection_path),
                        "--scope",
                        scope,
                    ],
                    cwd=ROOT,
                    env={**os.environ, "PYTHONPATH": str(BENCHMARK)},
                    check=True,
                    capture_output=True,
                    text=True,
                )
                outputs = dict(
                    line.split("=", 1) for line in result.stdout.splitlines()
                )
                self.assertEqual(json.loads(outputs["task_ids_json"]), expected_ids)
                self.assertEqual(int(outputs["count"]), len(expected_ids))
                self.assertEqual(
                    json.loads(outputs["tasks"]),
                    [slugs_by_id[task_id] for task_id in expected_ids],
                )

    def test_aggregate_is_published_from_completed_tasks_after_pair_failures(
        self,
    ) -> None:
        aggregate = self.workflow["jobs"]["aggregate-report"]
        combine = next(
            step
            for step in aggregate["steps"]
            if step.get("name") == "Combine reports without re-judging"
        )
        self.assertNotIn("if", combine)
        self.assertIn("--allow-missing", combine["run"])
        for step_name in (
            "Keep the Rust package manifest with the aggregate report",
            "Verify and archive the candidate identity",
            "Upload aggregate report",
        ):
            step = next(
                item for item in aggregate["steps"] if item.get("name") == step_name
            )
            self.assertNotIn("if", step)

    def test_workflow_calls_testable_retry_and_secret_scan_modules(self) -> None:
        workflow_text = WORKFLOW.read_text()
        self.assertIn("python -m zg_bench.reports.retries", workflow_text)
        self.assertIn("python -m zg_bench.ci.secret_scan", workflow_text)
        self.assertNotIn("python - <<'PY'", workflow_text)

    def test_reruns_validate_and_select_task_reports_before_aggregation(self) -> None:
        pair = self.workflow["jobs"]["run-pair"]["steps"]
        stamp = next(
            step for step in pair if "task_reports stamp" in step.get("run", "")
        )
        aggregate = self.workflow["jobs"]["aggregate-report"]["steps"]
        download = next(
            step for step in aggregate if step.get("with", {}).get("pattern")
        )
        self.assertEqual(
            download["with"]["pattern"], "swe-qa-task-report-*-${{ github.run_id }}-*"
        )
        self.assertNotEqual(download["with"].get("merge-multiple"), "true")
        combine = next(
            step
            for step in aggregate
            if step.get("name") == "Combine reports without re-judging"
        )
        self.assertEqual(stamp["env"], combine["env"])
        self.assertIn("github.workflow_sha", stamp["env"]["BENCH_HARNESS_COMMIT"])
        self.assertIn(
            "needs.package-candidate.outputs.candidate-commit",
            stamp["env"]["BENCH_CANDIDATE_COMMIT"],
        )
        self.assertIn("hashFiles(", stamp["env"]["BENCH_PROTOCOL_HASH"])
        self.assertLess(
            combine["run"].index("task_reports select"),
            combine["run"].index("swe_qa aggregate"),
        )
        self.assertIn(
            '--reports-root "$RUNNER_TEMP/selected-task-reports"', combine["run"]
        )
        self.assertIn("task-report-selection.json", combine["run"])


class OfflineWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.text = OFFLINE_WORKFLOW.read_text()
        cls.workflow = yaml.load(cls.text, Loader=yaml.BaseLoader)

    def test_pr_check_is_path_scoped_and_credential_free(self) -> None:
        triggers = self.workflow["on"]
        self.assertIn("pull_request", triggers)
        self.assertIn("workflow_dispatch", triggers)
        self.assertIn("benchmarks/swe-qa-bench/**", triggers["pull_request"]["paths"])
        self.assertNotIn("secrets.", self.text)
        self.assertNotIn("rust-candidate-package", self.text)

    def test_runs_python_and_shared_node_tests_without_building_candidate(self) -> None:
        commands = "\n".join(
            step.get("run", "") for step in self.workflow["jobs"]["test"]["steps"]
        )
        self.assertIn("python -m unittest discover", commands)
        self.assertIn("rust-package-cache.test.mjs", commands)
