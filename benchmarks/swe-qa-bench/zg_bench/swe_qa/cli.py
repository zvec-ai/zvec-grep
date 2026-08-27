"""Command line interface for the SWE-QA-Bench evaluation pipeline."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Sequence

from . import SweQaError
from .collect import collect_pair
from .judge import aggregate_reports, judge_pairs
from .validation import validate_assets


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="python -m zg_bench.swe_qa",
        description=(
            "Validate, collect, judge, and aggregate the locked SWE-QA-Bench subset."
        ),
    )
    commands = parser.add_subparsers(dest="command", required=True)

    validate = commands.add_parser("validate", help="validate pinned benchmark data")
    validate.add_argument("--selection", type=Path, required=True)
    validate.add_argument("--references", type=Path, required=True)
    validate.add_argument("--dataset", type=Path, required=True)

    collect = commands.add_parser("collect", help="collect one Harbor profile pair")
    collect.add_argument("--runs-dir", type=Path, required=True)
    collect.add_argument("--task", required=True)
    collect.add_argument("--output", type=Path, required=True)
    collect.add_argument("--expected-trials", type=int, default=1)

    judge = commands.add_parser("judge", help="judge pairs and render the report")
    judge.add_argument("--pairs-root", type=Path, required=True)
    judge.add_argument("--references", type=Path, required=True)
    judge.add_argument("--output-dir", type=Path, required=True)
    judge.add_argument("--expected", nargs="+", action="append", required=True)
    judge.add_argument("--attempts", type=int, default=3)

    aggregate = commands.add_parser(
        "aggregate", help="combine already judged per-task reports"
    )
    aggregate.add_argument("--reports-root", type=Path, required=True)
    aggregate.add_argument("--output-dir", type=Path, required=True)
    aggregate.add_argument("--expected", nargs="+", action="append")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        if args.command == "validate":
            result = validate_assets(
                selection_path=args.selection,
                references_path=args.references,
                dataset_path=args.dataset,
            )
            print(json.dumps(result, ensure_ascii=False))
        elif args.command == "collect":
            pair = collect_pair(
                runs_dir=args.runs_dir,
                task=args.task,
                output=args.output,
                expected_trials=args.expected_trials,
            )
            print(
                json.dumps(
                    {
                        "valid": True,
                        "task_id": pair["task_id"],
                        "expected_trials": pair["expected_trials"],
                        "actual_trials": pair["actual_trials"],
                        "output": str(args.output),
                    }
                )
            )
        elif args.command == "judge":
            expected = [task for group in args.expected for task in group]
            report = judge_pairs(
                pairs_root=args.pairs_root,
                references_path=args.references,
                output_dir=args.output_dir,
                expected=expected,
                attempts=args.attempts,
            )
            print(
                json.dumps(
                    {
                        "gate_passed": report["gate"]["passed"],
                        "report": str(args.output_dir / "report.json"),
                    }
                )
            )
        elif args.command == "aggregate":
            expected = (
                None
                if args.expected is None
                else [task for group in args.expected for task in group]
            )
            report = aggregate_reports(
                reports_root=args.reports_root,
                output_dir=args.output_dir,
                expected=expected,
            )
            print(
                json.dumps(
                    {
                        "gate_passed": report["gate"]["passed"],
                        "cases": len(report["cases"]),
                        "report": str(args.output_dir / "report.json"),
                    }
                )
            )
        else:  # pragma: no cover - argparse enforces this.
            raise SweQaError(f"unknown command: {args.command}")
    except SweQaError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0
