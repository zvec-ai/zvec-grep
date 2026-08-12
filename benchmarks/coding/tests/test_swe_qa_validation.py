from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from typing import Any

from zg_bench.swe_qa import SweQaError
from zg_bench.swe_qa.validation import validate_assets

CODING_DIR = Path(__file__).resolve().parents[1]
SELECTION_PATH = CODING_DIR / "zg_bench" / "swe_qa" / "data" / "selection.json"
REFERENCES_PATH = CODING_DIR / "zg_bench" / "swe_qa" / "data" / "references.json"
DATASET_PATH = CODING_DIR / "datasets" / "swe-qa-bench-manual"
EXPECTED_AUTO_TASK_IDS = (
    "reflex:6",
    "pylint:9",
    "matplotlib:37",
    "streamlink:14",
    "xarray:32",
)


class AutoSelectionValidationTests(unittest.TestCase):
    def _validate_selection(self, selection: dict[str, Any]) -> dict[str, Any]:
        with tempfile.TemporaryDirectory() as temp_dir:
            selection_path = Path(temp_dir) / "selection.json"
            selection_path.write_text(json.dumps(selection), encoding="utf-8")
            return validate_assets(
                selection_path=selection_path,
                references_path=REFERENCES_PATH,
                dataset_path=DATASET_PATH,
            )

    def test_checked_in_auto_selection_is_exact_and_valid(self) -> None:
        result = validate_assets(
            selection_path=SELECTION_PATH,
            references_path=REFERENCES_PATH,
            dataset_path=DATASET_PATH,
        )

        self.assertEqual(result["auto_task_count"], 5)
        self.assertEqual(tuple(result["auto_task_ids"]), EXPECTED_AUTO_TASK_IDS)

    def test_auto_selection_requires_exact_task_count(self) -> None:
        selection = json.loads(SELECTION_PATH.read_text(encoding="utf-8"))
        selection["gate"]["auto_tasks"].pop()

        with self.assertRaisesRegex(SweQaError, "exactly 5 tasks"):
            self._validate_selection(selection)

    def test_auto_selection_must_start_with_smoke(self) -> None:
        selection = json.loads(SELECTION_PATH.read_text(encoding="utf-8"))
        auto_tasks = selection["gate"]["auto_tasks"]
        auto_tasks[0], auto_tasks[1] = auto_tasks[1], auto_tasks[0]

        with self.assertRaisesRegex(SweQaError, "start with the configured smoke"):
            self._validate_selection(selection)

    def test_auto_selection_requires_all_four_categories_in_order(self) -> None:
        selection = json.loads(SELECTION_PATH.read_text(encoding="utf-8"))
        selection["gate"]["auto_tasks"][-1] = "sqlfluff:2"

        with self.assertRaisesRegex(SweQaError, "what/where/how/why order"):
            self._validate_selection(selection)

    def test_auto_selection_rejects_unknown_task(self) -> None:
        selection = json.loads(SELECTION_PATH.read_text(encoding="utf-8"))
        selection["gate"]["auto_tasks"][-1] = "unknown:0"

        with self.assertRaisesRegex(SweQaError, "unknown task IDs"):
            self._validate_selection(selection)


if __name__ == "__main__":
    unittest.main()
