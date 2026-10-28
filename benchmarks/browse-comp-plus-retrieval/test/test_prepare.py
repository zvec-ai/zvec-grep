from __future__ import annotations

import base64
import copy
import hashlib
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

import pyarrow as pa
from pyarrow import parquet

HERE = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "browsecomp_retrieval_prepare", HERE / "prepare.py"
)
prepare = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(prepare)


def encrypt(value, key=None):
    if key == "query_id":
        return value
    if isinstance(value, str):
        secret = hashlib.sha256(prepare.CANARY.encode()).digest()
        return base64.b64encode(
            bytes(b ^ secret[i % len(secret)] for i, b in enumerate(value.encode()))
        ).decode()
    if isinstance(value, list):
        return [encrypt(item) for item in value]
    if isinstance(value, dict):
        return {key: encrypt(item, key) for key, item in value.items()}
    return value


class PrepareTests(unittest.TestCase):
    def test_frozen_reference_preserves_ten_cases_and_gold_ids(self):
        selection = json.loads((HERE / "suites/ci-selection.json").read_text())
        lock = json.loads((HERE / "source.lock.json").read_text())
        ids = [row["query_id"] for row in selection["cases"]]
        self.assertEqual(
            ids, ["628", "700", "966", "827", "241", "165", "1068", "244", "843", "563"]
        )
        self.assertEqual(
            [
                line.strip()
                for line in (HERE / "suites/ci.txt").read_text().splitlines()
                if line.strip() and not line.lstrip().startswith("#")
            ],
            ids,
        )
        self.assertEqual(lock["queries"]["revision"], selection["dataset_revision"])
        self.assertEqual(lock["corpus"]["revision"], selection["corpus_revision"])
        self.assertEqual(
            lock["queries"]["decrypted_sha256"], selection["decrypted_queries_sha256"]
        )
        self.assertEqual(lock["queries"]["count"], 830)
        self.assertEqual(lock["corpus"]["count"], 100195)
        self.assertEqual(
            lock["reference"]["commit"], "c1299bf23ef4d06b3640880a1f0beed8cca092f8"
        )
        self.assertEqual(lock["model"], "local/potion-multilingual-128m")
        self.assertEqual(
            (lock["concurrency"], lock["repetitions"], lock["limit"]), (4, 5, 10)
        )
        for kind, count in [("queries", 6), ("corpus", 7)]:
            self.assertEqual(len(lock[kind]["files"]), count)
            for digest in lock[kind]["files"].values():
                self.assertRegex(digest, r"^[a-f0-9]{64}$")
        for row in selection["cases"]:
            rank = hashlib.sha256(
                f"{selection['dataset_revision']}\0{selection['seed']}\0{row['query_id']}".encode()
            ).hexdigest()
            self.assertEqual(rank, row["rank_sha256"])

    def fixture(self, root):
        rows = [
            {
                "query_id": str(i),
                "query": f"Original query {i}—文本\nunchanged?",
                "answer": "fixture answer",
                "gold_docs": [{"docid": str(i)}],
                "evidence_docs": [{"docid": str(i)}],
            }
            for i in range(1, 4)
        ]
        path = root / "queries.parquet"
        parquet.write_table(pa.Table.from_pylist([encrypt(row) for row in rows]), path)
        revision = "a" * 40
        ordered = sorted(
            rows,
            key=lambda row: hashlib.sha256(
                f"{revision}\0fixture\0{row['query_id']}".encode()
            ).hexdigest(),
        )
        selection = {
            "seed": "fixture",
            "cases": [
                {
                    "query_id": row["query_id"],
                    "gold_docids": [row["query_id"]],
                    "evidence_docids": [row["query_id"]],
                }
                for row in ordered[:2]
            ],
        }
        digest = hashlib.sha256(
            "".join(json.dumps(row, ensure_ascii=False) + "\n" for row in rows).encode()
        ).hexdigest()
        lock = {
            "queries": {"revision": revision, "count": 3, "decrypted_sha256": digest}
        }
        return path, rows, lock, selection

    def test_projected_parquet_decryption_preserves_original_query_bytes(self):
        with tempfile.TemporaryDirectory() as tmp:
            path, rows, lock, selection = self.fixture(Path(tmp))
            tasks = prepare.selected_tasks([path], lock, selection)
            self.assertEqual(
                [row["id"] for row in tasks],
                [row["query_id"] for row in selection["cases"]],
            )
            for task in tasks:
                original = next(row for row in rows if row["query_id"] == task["id"])
                self.assertEqual(task["query"].encode(), original["query"].encode())
                self.assertEqual(task["targets"], [{"path": task["id"] + ".md"}])
                self.assertNotIn("answer", task)

    def test_substituted_queries_gold_order_and_incomplete_sources_are_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            path, _, lock, selection = self.fixture(Path(tmp))
            for mutate in [
                lambda spec, _: spec["queries"].update(decrypted_sha256="0" * 64),
                lambda spec, _: spec["queries"].update(count=4),
                lambda _, audit: audit["cases"].reverse(),
                lambda _, audit: audit["cases"][0].update(gold_docids=["999"]),
            ]:
                spec, audit = copy.deepcopy(lock), copy.deepcopy(selection)
                mutate(spec, audit)
                with self.assertRaises(ValueError):
                    prepare.selected_tasks([path], spec, audit)
            with self.assertRaisesRegex(ValueError, "duplicate query"):
                prepare.selected_tasks([path, path], lock, selection)

    def test_document_materialization_preserves_text_without_wrappers(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            path = base / "source.parquet"
            rows = [
                {
                    "docid": "10",
                    "text": "# Original\r\nUnicode—文字\n",
                    "url": "https://example.invalid/one",
                },
                {
                    "docid": "2",
                    "text": "Body only.",
                    "url": "https://example.invalid/two",
                },
            ]
            parquet.write_table(pa.Table.from_pylist(rows), path)
            manifest = base / "manifest.jsonl"
            result = prepare.materialize([path], base / "corpus", manifest, 2)
            for row in rows:
                self.assertEqual(
                    (base / "corpus" / (row["docid"] + ".md")).read_bytes(),
                    row["text"].encode(),
                )
            self.assertEqual(
                result["bytes"], sum(len(row["text"].encode()) for row in rows)
            )
            self.assertEqual(result["manifest_sha256"], prepare.sha256_file(manifest))

    def test_unsafe_duplicate_and_incomplete_corpora_are_rejected(self):
        for ids, expected in [(["../outside"], 1), (["1", "1"], 2), (["1"], 2)]:
            with self.subTest(ids=ids), tempfile.TemporaryDirectory() as tmp:
                base = Path(tmp)
                path = base / "corpus.parquet"
                parquet.write_table(
                    pa.Table.from_pylist(
                        [
                            {
                                "docid": key,
                                "text": "fixture",
                                "url": "https://example.invalid",
                            }
                            for key in ids
                        ]
                    ),
                    path,
                )
                with self.assertRaises(ValueError):
                    prepare.materialize(
                        [path], base / "corpus", base / "manifest.jsonl", expected
                    )


if __name__ == "__main__":
    unittest.main()
