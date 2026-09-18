from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import yaml


ROOT = Path(__file__).resolve().parents[3]


class BenchmarkWorkflowTests(unittest.TestCase):
    def test_ci_scope_resolves_locked_tasks_and_five_trials_per_profile(self) -> None:
        workflow = yaml.load(
            (ROOT / ".github/workflows/swe-qa-bench.yml").read_text(),
            Loader=yaml.BaseLoader,
        )
        selection = json.loads(
            (ROOT / "benchmarks/swe-qa-bench/zg_bench/swe_qa/data/selection.json")
            .read_text()
        )
        script = next(
            step["run"]
            for step in workflow["jobs"]["validate"]["steps"]
            if step.get("id") == "task-matrix"
        )
        python_script = script.split("python - <<'PY'\n", 1)[1].rsplit("\nPY", 1)[0]
        default_scope = workflow["on"]["workflow_dispatch"]["inputs"]["scope"]["default"]
        self.assertEqual(default_scope, "repro-3")
        trials = int(workflow["env"]["SWE_QA_TRIALS_PER_PROFILE"])
        self.assertEqual(trials, 5)
        tasks_by_id = {task["task_id"]: task for task in selection["tasks"]}
        scopes = {
            "all-full": list(tasks_by_id),
            "gate-20": list(tasks_by_id),
            "smoke": selection["gate"]["auto_tasks"],
            "repro-3": ["reflex:6", "requests:16", "conan:39"],
        }
        for scope, expected_ids in scopes.items():
            with self.subTest(scope=scope), tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / "outputs"
                subprocess.run(
                    [sys.executable, "-c", python_script],
                    cwd=ROOT,
                    env={"SCOPE": scope, "GITHUB_OUTPUT": str(output)},
                    check=True,
                    capture_output=True,
                    text=True,
                )
                values = dict(line.split("=", 1) for line in output.read_text().splitlines())
                self.assertEqual(json.loads(values["task_ids_json"]), expected_ids)
                self.assertEqual(
                    json.loads(values["tasks"]),
                    [tasks_by_id[task_id]["task_slug"] for task_id in expected_ids],
                )
                count = int(values["count"])
                self.assertEqual(count, len(expected_ids))
                self.assertEqual(count * 2 * trials, len(expected_ids) * 10)
