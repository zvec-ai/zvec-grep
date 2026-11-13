"""The opt-in symbol suite stays isolated from QA20 and judge-only evidence."""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import yaml

from zg_bench.core.errors import SweQaError
from zg_bench.runner import SuiteConfigError, build_harbor_command, load_suite
from zg_bench.swe_qa.selection import matrix_outputs, select_ci_tasks
from zg_bench.swe_qa.validation_symbols import SMOKE_TASK_IDS, TASK_IDS, validate_symbol_assets

ROOT = Path(__file__).resolve().parents[1]
DATA = ROOT / "zg_bench/swe_qa/data"
SELECTION = DATA / "selection-symbols.json"
REFERENCES = DATA / "references-symbols.json"
DATASET = ROOT / "datasets-symbols"


class SymbolAssetTests(unittest.TestCase):
    def validate(self, selection=SELECTION, references=REFERENCES, dataset=DATASET):
        return validate_symbol_assets(selection_path=selection, references_path=references, dataset_path=dataset)

    def test_fixed_assets_and_shared_matrix_scopes(self):
        self.assertTrue(self.validate()["valid"])
        selection = json.loads(SELECTION.read_text())
        self.assertEqual(select_ci_tasks(selection, "smoke")[0], list(SMOKE_TASK_IDS))
        self.assertEqual(select_ci_tasks(selection, "all-full")[0], list(TASK_IDS))
        for scope, tier in (("smoke", "smoke"), ("all-full", "full")):
            _, slugs = select_ci_tasks(selection, scope)
            self.assertEqual(list(load_suite("caller-callee-symbols", tier=tier).tasks), slugs)
        self.assertTrue(load_suite("caller-callee-symbols").graph_index)
        self.assertFalse(load_suite("swe-qa-bench").graph_index)

    def test_bad_hash_and_source_provenance_are_rejected(self):
        for key, value, message in (("question_hash", "0" * 64, "SHA256"), ("provenance", "upstream", "self-authored")):
            with self.subTest(field=key), tempfile.TemporaryDirectory() as temp:
                selection = json.loads(SELECTION.read_text())
                selection["tasks"][0][key] = value
                path = Path(temp) / "selection.json"
                path.write_text(json.dumps(selection))
                with self.assertRaisesRegex(SweQaError, message):
                    self.validate(selection=path)

    def test_missing_and_leaked_judge_evidence_are_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            dataset = Path(temp) / "dataset"
            shutil.copytree(DATASET, dataset)
            instruction = dataset / "requests-cc1/instruction.md"
            instruction.unlink()
            with self.assertRaisesRegex(SweQaError, "could not read"):
                self.validate(dataset=dataset)
            shutil.copyfile(DATASET / "requests-cc1/instruction.md", instruction)
            gold = json.loads(REFERENCES.read_text())["references"][0]["reference_answer"]
            (dataset / "requests-cc1/tests/leaked.txt").write_text(gold)
            with self.assertRaisesRegex(SweQaError, "judge-only reference leaked"):
                self.validate(dataset=dataset)

    def test_suite_graph_flag_reaches_only_the_zvec_profile(self):
        suite = load_suite("caller-callee-symbols", tier="full")
        for profile in ("baseline", "zvec-grep"):
            command = build_harbor_command(suite=suite, profile=profile, agent="opencode", model="custom-openai/glm-5.2", job_name="symbol-fixture")
            self.assertEqual("graph_index=true" in command, profile == "zvec-grep")
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "suite.yaml"
            path.write_text((ROOT / "suites/caller-callee-symbols.yaml").read_text().replace("path: ../datasets-symbols", f"path: {DATASET}").replace("graph_index: true", "graph_index: 1").replace("index_ignore_file: ../zg_bench/swe_qa/data/index-symbols.ignore", f"index_ignore_file: {DATA / 'index-symbols.ignore'}"))
            with self.assertRaisesRegex(SuiteConfigError, "graph_index must be a boolean"):
                load_suite(path)

    def test_workflow_routes_each_dataset_and_rejects_wrong_scope(self):
        workflow = yaml.load((ROOT.parents[1] / ".github/workflows/swe-qa-bench.yml").read_text(), Loader=yaml.BaseLoader)
        resolver = next(s for s in workflow["jobs"]["validate"]["steps"] if s.get("id") == "task-set")
        for task_set, scope, valid in (("qa20", "repro-3", True), ("caller-callee-symbols", "smoke", True), ("caller-callee-symbols", "all-full", True), ("caller-callee-symbols", "repro-3", False), ("unknown", "smoke", False)):
            with self.subTest(task_set=task_set, scope=scope), tempfile.TemporaryDirectory() as temp:
                output = Path(temp) / "output"
                result = subprocess.run(["bash", "-c", resolver["run"]], cwd=ROOT.parents[1], env={**os.environ, "TASK_SET": task_set, "SCOPE": scope, "GITHUB_OUTPUT": str(output)}, capture_output=True, text=True)
                self.assertEqual(result.returncode == 0, valid, result.stdout + result.stderr)
                if valid:
                    values = dict(line.split("=", 1) for line in output.read_text().splitlines())
                    self.assertEqual(values["suite"], "swe-qa-bench" if task_set == "qa20" else task_set)
                    matrix_outputs(ROOT.parents[1] / values["selection"], scope)
                    command = [sys.executable, "-m", "zg_bench.swe_qa", values["validator"], "--selection", values["selection"], "--references", values["references"], "--dataset", values["dataset"]]
                    subprocess.run(command, cwd=ROOT.parents[1], check=True, capture_output=True)


if __name__ == "__main__":
    unittest.main()
