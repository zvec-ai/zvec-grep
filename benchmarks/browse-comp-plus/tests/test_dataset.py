from __future__ import annotations

import base64
import hashlib
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import pyarrow as arrow
from huggingface_hub.utils import build_hf_headers
from pyarrow import parquet
from zg_bench.config import load_config
from zg_bench.dataset import CANARY, _decrypt, decrypt_queries, fetch, load_queries


class DatasetAuthenticationTests(unittest.TestCase):
    def test_unconfigured_optional_token_downloads_both_snapshots_anonymously(self):
        for token in ("", " \n\t"):
            with self.subTest(token=token), tempfile.TemporaryDirectory() as root:
                with (
                    patch("zg_bench.dataset.snapshot_download") as download,
                    self.assertRaisesRegex(RuntimeError, "Parquet"),
                ):
                    fetch(load_config(), Path(root), token=token)
                self.assertEqual(download.call_count, 2)
                for call in download.call_args_list:
                    self.assertIs(call.kwargs["token"], False)
                    self.assertNotIn(
                        "authorization", build_hf_headers(token=call.kwargs["token"])
                    )

    def test_configured_token_is_preserved_and_missing_argument_uses_hub_defaults(self):
        for token, expected in (("hf_fixture", "hf_fixture"), (None, None)):
            with self.subTest(token=token), tempfile.TemporaryDirectory() as root:
                with (
                    patch("zg_bench.dataset.snapshot_download") as download,
                    self.assertRaisesRegex(RuntimeError, "Parquet"),
                ):
                    fetch(load_config(), Path(root), token=token)
                self.assertEqual(download.call_count, 2)
                for call in download.call_args_list:
                    self.assertEqual(call.kwargs["token"], expected)


class QueryProjectionTests(unittest.TestCase):
    @staticmethod
    def encrypt(value, *, key=None):
        if key == "query_id":
            return value
        if isinstance(value, str):
            encoded = value.encode("utf-8")
            password = hashlib.sha256(CANARY.encode("utf-8")).digest()
            return base64.b64encode(
                bytes(
                    byte ^ password[index % len(password)]
                    for index, byte in enumerate(encoded)
                )
            ).decode("ascii")
        if isinstance(value, list):
            return [QueryProjectionTests.encrypt(item) for item in value]
        if isinstance(value, dict):
            return {
                name: QueryProjectionTests.encrypt(item, key=name)
                for name, item in value.items()
            }
        return value

    @staticmethod
    def row(index):
        return {
            "query_id": str(index),
            "query": f"Synthetic question {index} — 中文 fixture",
            "answer": f"Synthetic answer {index}",
            "gold_docs": [
                {"docid": f"gold-{index}", "text": "Unused gold body", "url": "unused"},
                {"docid": "shared-gold", "text": "Another body", "url": "unused"},
            ],
            "evidence_docs": [
                {
                    "docid": f"evidence-{index}",
                    "text": "Unused evidence",
                    "url": "unused",
                },
            ],
            "negative_docs": [
                {
                    "docid": "negative",
                    "text": "Unused negative body" * 10,
                    "url": "unused",
                },
            ],
        }

    def test_real_nested_parquet_projection_preserves_all_consumed_semantics(self):
        rows = [self.row(index) for index in range(3)]
        rows[1]["gold_docs"] = []
        rows[2]["evidence_docs"] = []
        encrypted = [self.encrypt(row) for row in rows]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "queries.parquet"
            parquet.write_table(arrow.Table.from_pylist(encrypted), source)
            legacy = root / "legacy.jsonl"
            legacy.write_text(
                "".join(
                    json.dumps(_decrypt(row), ensure_ascii=False) + "\n"
                    for row in encrypted
                ),
                encoding="utf-8",
            )
            output = root / "projected.jsonl"
            progress = []
            count, digest = decrypt_queries(
                [source],
                output,
                expected=3,
                progress=lambda current, total: progress.append((current, total)),
            )
            self.assertEqual(count, 3)
            self.assertEqual(digest, hashlib.sha256(output.read_bytes()).hexdigest())
            self.assertEqual(progress, [(0, 3), (1, 3), (2, 3), (3, 3)])
            self.assertEqual(load_queries(output), load_queries(legacy))
            self.assertLess(output.stat().st_size, legacy.stat().st_size)
            for line in output.read_text(encoding="utf-8").splitlines():
                row = json.loads(line)
                self.assertEqual(
                    set(row),
                    {
                        "query_id",
                        "query",
                        "answer",
                        "gold_docs",
                        "evidence_docs",
                    },
                )
                for name in ("gold_docs", "evidence_docs"):
                    self.assertTrue(
                        all(set(document) == {"docid"} for document in row[name])
                    )

    def test_unused_document_fields_are_never_decrypted(self):
        row = self.encrypt(self.row(1))
        for name in ("gold_docs", "evidence_docs", "negative_docs"):
            for document in row[name]:
                document["text"] = "invalid ciphertext!"
                document["url"] = "invalid ciphertext!"
                if name == "negative_docs":
                    document["docid"] = "invalid ciphertext!"
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "queries.parquet"
            output = Path(directory) / "queries.jsonl"
            parquet.write_table(arrow.Table.from_pylist([row]), source)
            count, _ = decrypt_queries([source], output)
            self.assertEqual(count, 1)
            self.assertEqual(
                load_queries(output)[0]["gold_docs"], ["gold-1", "shared-gold"]
            )
            self.assertNotIn("invalid ciphertext", output.read_text())

    def test_all_830_queries_are_kept_across_six_parquet_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            files = []
            offset = 0
            for part, count in enumerate((139, 139, 138, 138, 138, 138)):
                path = root / f"test-{part:05d}-of-00006.parquet"
                parquet.write_table(
                    arrow.Table.from_pylist(
                        [
                            self.encrypt(self.row(index))
                            for index in range(offset, offset + count)
                        ]
                    ),
                    path,
                )
                files.append(path)
                offset += count
            output = root / "queries.jsonl"
            count, _ = decrypt_queries(files, output, expected=830)
            loaded = load_queries(output)
            self.assertEqual(count, 830)
            self.assertEqual(
                [row["query_id"] for row in loaded], list(map(str, range(830)))
            )
            self.assertEqual(loaded[-1]["answer"], "Synthetic answer 829")
            self.assertEqual(loaded[-1]["evidence_docs"], ["evidence-829"])
