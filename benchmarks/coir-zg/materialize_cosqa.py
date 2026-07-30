#!/usr/bin/env python3
# /// script
# requires-python = ">=3.12,<3.14"
# dependencies = ["datasets==4.5.0"]
# ///
"""Materialize the pinned CoIR CosQA dataset as files for zvec-grep."""

from __future__ import annotations

import argparse
import hashlib
import json
from importlib.metadata import version
from pathlib import Path

from datasets import load_dataset


SCRIPT_DIR = Path(__file__).resolve().parent
DATASET_REPO = "CoIR-Retrieval/cosqa-queries-corpus"
DATASET_REVISION = "d56676dfbe7cd137229c33bd1e7dd96c688d2126"
QRELS_REPO = "CoIR-Retrieval/cosqa-qrels"
QRELS_REVISION = "c70cfe89508993ed4707e31be1f83908f1fd6d38"
EXPECTED_CORPUS_DOCUMENTS = 20_604
EXPECTED_TEST_QUERIES = 500
EXPECTED_CORPUS_SHA256 = (
    "753082a57c28ef708ccf1fe327067b99a96c04e4383921be9099742a5f681fac"
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Materialize the pinned CoIR CosQA test corpus for CoIR-ZG."
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=SCRIPT_DIR / "work" / "data" / "cosqa",
        help="output directory (default: benchmarks/coir-zg/work/data/cosqa)",
    )
    return parser.parse_args()


def write_jsonl(path: Path, rows: list[dict]) -> None:
    with path.open("w", encoding="utf-8") as handle:
        for row in rows:
            handle.write(json.dumps(row, ensure_ascii=False, separators=(",", ":")))
            handle.write("\n")


def main() -> None:
    args = parse_args()
    output = args.output.resolve()
    corpus_dir = output / "corpus"
    manifest_path = output / "manifest.jsonl"
    queries_path = output / "queries.jsonl"
    qrels_path = output / "qrels.jsonl"
    metadata_path = output / "metadata.json"

    if any(path.exists() for path in (manifest_path, queries_path, qrels_path)):
        raise SystemExit(
            f"{output} already contains materialized data; refusing to overwrite it"
        )

    output.mkdir(parents=True, exist_ok=True)
    corpus_dir.mkdir(parents=True, exist_ok=True)

    query_corpus = load_dataset(DATASET_REPO, revision=DATASET_REVISION)
    test_qrels = load_dataset(QRELS_REPO, revision=QRELS_REVISION)["test"]

    qrel_rows = [
        {
            "query_id": str(row["query_id"]),
            "corpus_id": str(row["corpus_id"]),
            "score": int(row["score"]),
        }
        for row in test_qrels
    ]
    test_query_ids = {row["query_id"] for row in qrel_rows}
    query_by_id = {
        str(row["_id"]): row
        for row in query_corpus["queries"]
        if str(row["_id"]) in test_query_ids
    }

    missing_queries = sorted(test_query_ids - query_by_id.keys())
    if missing_queries:
        raise RuntimeError(f"Missing test queries: {missing_queries[:10]}")

    manifest_rows: list[dict] = []
    corpus_hash = hashlib.sha256()

    for index, row in enumerate(query_corpus["corpus"], start=1):
        doc_id = str(row["_id"])
        relative_path = f"{index:08d}.py"
        text = str(row["text"])
        encoded = text.encode("utf-8")
        digest = hashlib.sha256(encoded).hexdigest()

        (corpus_dir / relative_path).write_bytes(encoded)
        corpus_hash.update(doc_id.encode("utf-8"))
        corpus_hash.update(b"\0")
        corpus_hash.update(encoded)
        corpus_hash.update(b"\0")
        manifest_rows.append(
            {
                "path": relative_path,
                "doc_id": doc_id,
                "partition": str(row["partition"]),
                "language": str(row["language"]),
                "bytes": len(encoded),
                "sha256": digest,
            }
        )

    query_rows = [
        {
            "query_id": qrel["query_id"],
            "text": str(query_by_id[qrel["query_id"]]["text"]),
        }
        for qrel in qrel_rows
    ]
    corpus_sha256 = corpus_hash.hexdigest()

    if len(manifest_rows) != EXPECTED_CORPUS_DOCUMENTS:
        raise RuntimeError(
            f"Expected {EXPECTED_CORPUS_DOCUMENTS} corpus documents, "
            f"found {len(manifest_rows)}"
        )
    if len(query_rows) != EXPECTED_TEST_QUERIES:
        raise RuntimeError(
            f"Expected {EXPECTED_TEST_QUERIES} test queries, found {len(query_rows)}"
        )
    if corpus_sha256 != EXPECTED_CORPUS_SHA256:
        raise RuntimeError(
            f"Corpus checksum mismatch: expected {EXPECTED_CORPUS_SHA256}, "
            f"found {corpus_sha256}"
        )

    write_jsonl(manifest_path, manifest_rows)
    write_jsonl(queries_path, query_rows)
    write_jsonl(qrels_path, qrel_rows)
    metadata_path.write_text(
        json.dumps(
            {
                "benchmark": "CoIR-ZG",
                "task": "cosqa",
                "dataset_repo": DATASET_REPO,
                "dataset_revision": DATASET_REVISION,
                "qrels_repo": QRELS_REPO,
                "qrels_revision": QRELS_REVISION,
                "qrels_split": "test",
                "datasets_version": version("datasets"),
                "corpus_documents": len(manifest_rows),
                "queries": len(query_rows),
                "positive_qrels": sum(row["score"] > 0 for row in qrel_rows),
                "corpus_sha256": corpus_sha256,
            },
            ensure_ascii=False,
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )

    print(metadata_path.read_text(encoding="utf-8"), end="")


if __name__ == "__main__":
    main()
