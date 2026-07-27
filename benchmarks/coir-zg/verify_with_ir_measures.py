#!/usr/bin/env python3
# /// script
# requires-python = ">=3.12,<3.14"
# dependencies = ["ir-measures==0.4.1"]
# ///
"""Verify CoIR-ZG metrics with the external ir-measures evaluator."""

from __future__ import annotations

import argparse
import json
import math
from pathlib import Path

from ir_measures import AP, RR, R, nDCG, Qrel, ScoredDoc, calc_aggregate


SCRIPT_DIR = Path(__file__).resolve().parent


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Recompute CoIR-ZG metrics with ir-measures and compare them."
    )
    parser.add_argument(
        "--dataset",
        type=Path,
        default=SCRIPT_DIR / "work" / "data" / "cosqa",
        help="materialized dataset directory",
    )
    parser.add_argument(
        "--results",
        type=Path,
        default=SCRIPT_DIR / "work" / "results" / "cosqa",
        help="benchmark result directory",
    )
    parser.add_argument(
        "--tolerance",
        type=float,
        default=1e-12,
        help="absolute metric comparison tolerance (default: 1e-12)",
    )
    return parser.parse_args()


def read_jsonl(path: Path) -> list[dict]:
    with path.open(encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


def main() -> None:
    args = parse_args()
    qrel_rows = read_jsonl(args.dataset / "qrels.jsonl")
    measures = [nDCG @ 10, R @ 10, AP @ 10, nDCG @ 100, R @ 100, AP @ 100, RR @ 10]
    failures: list[str] = []
    verified = 0

    for model_dir in sorted(path for path in args.results.iterdir() if path.is_dir()):
        ranking_path = model_dir / "rankings.jsonl"
        metrics_path = model_dir / "metrics.json"
        if not ranking_path.exists() or not metrics_path.exists():
            continue

        ranking_rows = read_jsonl(ranking_path)
        query_ids = {row["query_id"] for row in ranking_rows}
        qrels = [
            Qrel(row["query_id"], row["corpus_id"], int(row["score"]))
            for row in qrel_rows
            if row["query_id"] in query_ids
        ]
        run = []
        for row in ranking_rows:
            documents = row["documents"]
            for rank, doc_id in enumerate(documents):
                run.append(
                    ScoredDoc(
                        row["query_id"],
                        doc_id,
                        float(len(documents) - rank),
                    )
                )

        aggregate = calc_aggregate(measures, qrels, run)
        normalized = {
            "nDCG@10": aggregate[nDCG @ 10],
            "Recall@10": aggregate[R @ 10],
            "MAP@10": aggregate[AP @ 10],
            "nDCG@100": aggregate[nDCG @ 100],
            "Recall@100": aggregate[R @ 100],
            "MAP@100": aggregate[AP @ 100],
            "MRR@10": aggregate[RR @ 10],
        }
        metrics_report = json.loads(metrics_path.read_text(encoding="utf-8"))
        expected = metrics_report["metrics"]

        model_failures = [
            f"{name}: internal={expected[name]:.16g}, ir-measures={score:.16g}"
            for name, score in normalized.items()
            if not math.isclose(
                expected[name],
                score,
                rel_tol=0,
                abs_tol=args.tolerance,
            )
        ]
        if metrics_report["query_count"] != len(query_ids):
            model_failures.append(
                f"query_count: metrics={metrics_report['query_count']}, "
                f"rankings={len(query_ids)}"
            )
        if model_failures:
            failures.extend(
                f"{model_dir.name}: {failure}" for failure in model_failures
            )
            print(f"{model_dir.name}: FAILED")
        else:
            verified += 1
            print(f"{model_dir.name}: verified")

        output_path = model_dir / "ir-measures.json"
        output_path.write_text(
            json.dumps(normalized, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )

    if verified == 0 and not failures:
        raise SystemExit(f"No complete result directories found under {args.results}")
    if failures:
        raise SystemExit("Metric verification failed:\n" + "\n".join(failures))

    print(f"Verified {verified} model result(s).")


if __name__ == "__main__":
    main()
