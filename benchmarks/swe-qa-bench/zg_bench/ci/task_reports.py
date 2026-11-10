"""Bind task reports to a CI run and select valid results across its attempts."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import sys
from pathlib import Path
from typing import Any, Mapping, Sequence

from zg_bench.core.errors import SweQaError
from zg_bench.core.io import load_object
from zg_bench.reports.validation import validate_task_report

MANIFEST = "task-report-manifest.json"


def _digest(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def run_identity(env: Mapping[str, str]) -> dict[str, Any]:
    """Use only public configuration; never include API keys in artifacts."""
    required = (
        "GITHUB_RUN_ID",
        "GITHUB_RUN_ATTEMPT",
        "BENCH_CANDIDATE_COMMIT",
        "BENCH_CANDIDATE_REPOSITORY",
        "BENCH_HARNESS_COMMIT",
        "BENCH_PROTOCOL_HASH",
        "BENCH_SCOPE",
        "SWE_QA_MODEL",
        "BENCH_EMBEDDING_RUNTIME",
        "BENCH_EMBEDDING_MODEL",
    )
    if any(not env.get(key) for key in required):
        raise SweQaError("missing task report run identity/configuration")
    for key in ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT"):
        if not re.fullmatch(r"[1-9][0-9]*", env[key]):
            raise SweQaError(f"invalid {key}")
    for key in ("BENCH_CANDIDATE_COMMIT", "BENCH_HARNESS_COMMIT"):
        if not re.fullmatch(r"[a-f0-9]{40}", env[key]):
            raise SweQaError(f"invalid {key}")
    if not re.fullmatch(r"[a-f0-9]{64}", env["BENCH_PROTOCOL_HASH"]):
        raise SweQaError("invalid BENCH_PROTOCOL_HASH")
    if not re.fullmatch(r"[\w.-]+/[\w.-]+", env["BENCH_CANDIDATE_REPOSITORY"]):
        raise SweQaError("invalid candidate repository")
    config = load_object(
        Path(__file__).parents[2] / "ci-config.json", label="CI config"
    )
    runtime = env["BENCH_EMBEDDING_RUNTIME"]
    if config["embedding_models"].get(runtime) != env["BENCH_EMBEDDING_MODEL"]:
        raise SweQaError("embedding runtime/model disagrees with CI config")
    configuration = {
        "scope": env["BENCH_SCOPE"],
        "agent": "opencode",
        "model": env["SWE_QA_MODEL"],
        "embedding_runtime": runtime,
        "embedding_model": env["BENCH_EMBEDDING_MODEL"],
        "trials_per_profile": config["trials_per_profile"],
        "max_retries": config["max_retries"],
        "model_endpoint_sha256": _digest(env.get("SWE_QA_MODEL_BASE_URL", "").encode()),
        "embedding_endpoint_sha256": _digest(
            (
                env.get("BENCH_EMBEDDING_ENDPOINT", "") if runtime == "remote" else ""
            ).encode()
        ),
    }
    return {
        "schema_version": 1,
        "run_id": env["GITHUB_RUN_ID"],
        "run_attempt": int(env["GITHUB_RUN_ATTEMPT"]),
        "candidate_commit": env["BENCH_CANDIDATE_COMMIT"],
        "candidate_repository": env["BENCH_CANDIDATE_REPOSITORY"],
        "harness_commit": env["BENCH_HARNESS_COMMIT"],
        "protocol_sha256": env["BENCH_PROTOCOL_HASH"],
        "configuration": configuration,
    }


def _validated_task(report_path: Path, identity: dict[str, Any]) -> str:
    report = load_object(report_path, label="per-task report")
    case = validate_task_report(report, report_path)
    config = identity["configuration"]
    if report["judge"]["model"] != config["model"]:
        raise SweQaError("task report uses a different judge model")
    if case.get("trial_count") != config["trials_per_profile"]:
        raise SweQaError("task report uses a different trial count")
    return case["task_id"]


def stamp_report(directory: Path, identity: dict[str, Any]) -> None:
    report_path = directory / "report.json"
    task_id = _validated_task(report_path, identity)
    manifest = {
        **identity,
        "task_id": task_id,
        "report_sha256": _digest(report_path.read_bytes()),
    }
    (directory / MANIFEST).write_text(json.dumps(manifest, indent=2) + "\n")


def select_reports(
    *,
    reports_root: Path,
    output_dir: Path,
    identity: dict[str, Any],
    expected: Sequence[str],
) -> dict[str, Any]:
    """Keep the newest valid report per task; preserve reasons for exclusions."""
    if not expected or len(set(expected)) != len(expected):
        raise SweQaError("expected task IDs must be non-empty and unique")
    selected: dict[str, tuple[int, Path]] = {}
    seen: set[tuple[str, int]] = set()
    ignored = []
    for report_path in sorted(reports_root.rglob("report.json")):
        try:
            manifest = load_object(report_path.parent / MANIFEST, label="task manifest")
            if any(
                manifest.get(key) != value
                for key, value in identity.items()
                if key != "run_attempt"
            ):
                raise SweQaError(
                    "task report candidate/run/protocol/configuration mismatch"
                )
            attempt = manifest.get("run_attempt")
            if type(attempt) is not int or not 1 <= attempt <= identity["run_attempt"]:
                raise SweQaError("task report has an invalid or future attempt")
            if manifest.get("report_sha256") != _digest(report_path.read_bytes()):
                raise SweQaError("task report checksum mismatch")
            task_id = _validated_task(report_path, identity)
            if manifest.get("task_id") != task_id or task_id not in expected:
                raise SweQaError("task report is outside the locked task selection")
        except (SweQaError, OSError) as error:
            ignored.append({"report": str(report_path), "reason": str(error)})
            continue
        previous = selected.get(task_id)
        if (task_id, attempt) in seen:
            raise SweQaError(
                f"duplicate task report for {task_id} in attempt {attempt}"
            )
        seen.add((task_id, attempt))
        if not previous or attempt > previous[0]:
            selected[task_id] = (attempt, report_path)

    output_dir.mkdir(parents=True, exist_ok=False)
    sources = []
    for index, task_id in enumerate(expected):
        if task_id not in selected:
            continue
        attempt, source = selected[task_id]
        destination = output_dir / f"task-{index + 1}"
        destination.mkdir()
        for name in ("report.json", MANIFEST):
            shutil.copyfile(source.parent / name, destination / name)
        sources.append({"task_id": task_id, "attempt": attempt, "report": str(source)})
    selection = {
        **identity,
        "selected": sources,
        "missing_tasks": [task for task in expected if task not in selected],
        "ignored": ignored,
    }
    (output_dir / "selection.json").write_text(json.dumps(selection, indent=2) + "\n")
    if not selected:
        raise SweQaError(
            "no valid task reports match this candidate and evaluation configuration"
        )
    return selection


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    stamp = commands.add_parser("stamp")
    stamp.add_argument("--report-dir", type=Path, required=True)
    select = commands.add_parser("select")
    select.add_argument("--reports-root", type=Path, required=True)
    select.add_argument("--output-dir", type=Path, required=True)
    select.add_argument("--expected", nargs="+", required=True)
    args = parser.parse_args(argv)
    try:
        identity = run_identity(os.environ)
        if args.command == "stamp":
            stamp_report(args.report_dir, identity)
        else:
            result = select_reports(
                reports_root=args.reports_root,
                output_dir=args.output_dir,
                identity=identity,
                expected=args.expected,
            )
            print(json.dumps(result))
    except (SweQaError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
