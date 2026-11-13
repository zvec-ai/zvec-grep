"""Frozen SWE-QA selection, provenance and reference isolation contracts."""

from __future__ import annotations

import json
import shutil
import tempfile
import unittest
from pathlib import Path

from swe_qa_fixtures import (
    DATASET_PATH,
    EXPECTED_TASK_IDS,
    REFERENCES_PATH,
    SELECTION_PATH,
    _write_json,
)
from swe_qa_fixtures import (
    setUpModule as setUpModule,
)
from swe_qa_fixtures import (
    tearDownModule as tearDownModule,
)
from zg_bench.swe_qa import SweQaError
from zg_bench.swe_qa.validation import validate_assets


class ValidationTests(unittest.TestCase):
    def test_checked_in_selection_references_and_dataset_validate(self) -> None:
        result = validate_assets(
            selection_path=SELECTION_PATH,
            references_path=REFERENCES_PATH,
            dataset_path=DATASET_PATH,
        )

        self.assertTrue(result["valid"])
        self.assertEqual(result["task_count"], 23)
        callee_tasks = tuple(json.loads(SELECTION_PATH.read_text())["gate"]["callees_tasks"])
        self.assertEqual(tuple(result["task_ids"]), EXPECTED_TASK_IDS + ("requests:45", "sympy:37") + callee_tasks)
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

    def test_auto_category_mismatch_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            selection = json.loads(SELECTION_PATH.read_text())
            selection["tasks"][4]["category"] = "where"
            selection["tasks"][4]["question_type"] = "where"
            selection_path = Path(temp_dir) / "selection.json"
            _write_json(selection_path, selection)

            with self.assertRaisesRegex(SweQaError, "what/where/how/why order"):
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
