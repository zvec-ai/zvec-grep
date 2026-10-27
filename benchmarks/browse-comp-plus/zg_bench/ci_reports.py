"""Small retained reports and retry diagnostics for BrowseComp-Plus CI."""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import stat
import sys
from pathlib import Path
from typing import Any

from .config import DEFAULT_ARTIFACTS_DIR, load_config
from .models import PROFILES

PUBLIC_FILES = ("benchmark.toml", "candidate-manifest.json", "ci.json", "plan.json")
FAILED_STATUSES = {"failed", "infrastructure_failed"}


def _object(path: Path) -> dict[str, Any]:
    """Incomplete diagnostics are expected after a failed or interrupted job."""
    try:
        if path.is_symlink() or not path.is_file():
            return {}
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return value if isinstance(value, dict) else {}


def _directories(root: Path) -> list[Path]:
    if root.is_symlink() or not root.is_dir():
        return []
    return sorted(
        path for path in root.iterdir() if not path.is_symlink() and path.is_dir()
    )


def _status(path: Path) -> str | None:
    value = _object(path).get("status")
    return value if isinstance(value, str) else None


def retry_counts(artifacts: Path) -> dict[str, dict[str, int]]:
    """Count final selected trials separately from their raw attempt overhead."""
    counts = {
        profile: {
            "planned": 0,
            "successful": 0,
            "paired": 0,
            "retries": 0,
            "failed_attempts": 0,
            "final_failures": 0,
            "unfinished": 0,
        }
        for profile in PROFILES
    }
    runs = [
        run
        for run in _directories(artifacts / "runs")
        if not _object(run / "run.json").get("aggregate_only")
    ]
    if not runs:
        return counts
    # A worker has one run; local use can retain older runs. Never sum old runs.
    run = max(
        runs,
        key=lambda path: (
            (path / "run.json").stat().st_mtime_ns
            if (path / "run.json").is_file()
            else path.stat().st_mtime_ns,
            path.name,
        ),
    )
    metadata = _object(run / "run.json")
    query_ids = metadata.get("query_ids")
    per_case = metadata.get("trials_per_case")
    planned = (
        len(query_ids) * per_case
        if isinstance(query_ids, list) and type(per_case) is int and per_case > 0
        else 0
    )
    observed = {profile: 0 for profile in PROFILES}
    for case in _directories(run / "cases"):
        pair_trials = _object(case / "pair.json").get("trials", [])
        paired = (
            {
                item["trial_index"]
                for item in pair_trials
                if isinstance(item, dict)
                and item.get("eligible") is True
                and type(item.get("trial_index")) is int
            }
            if isinstance(pair_trials, list)
            else set()
        )
        for profile in PROFILES:
            profile_root = case / profile
            if profile_root.is_symlink():
                continue
            for trial in _directories(profile_root / "trials"):
                match = re.fullmatch(r"trial-([0-9]+)", trial.name)
                if not match:
                    continue
                observed[profile] += 1
                row = counts[profile]
                attempts = [
                    path
                    for path in _directories(trial / "attempts")
                    if re.fullmatch(r"attempt-[0-9]+", path.name)
                ]
                row["retries"] += max(0, len(attempts) - 1)
                row["failed_attempts"] += sum(
                    _status(attempt / "result.json") in FAILED_STATUSES
                    for attempt in attempts
                )
                # Only result.json selects the attempt used by paired reports.
                status = _status(trial / "result.json")
                row["successful"] += status == "completed"
                row["paired"] += status == "completed" and int(match[1]) in paired
                row["final_failures"] += status in FAILED_STATUSES
    for profile, row in counts.items():
        row["planned"] = max(planned, observed[profile])
        row["unfinished"] = row["planned"] - row["successful"] - row["final_failures"]
    return counts


def render_retry_summary(artifacts: Path) -> str:
    config = load_config()
    lines = [
        "## Trial retries",
        "",
        (
            f"Each profile has {config.run.trials_per_case} trials per case. "
            f"Infrastructure failures receive at most {config.run.infrastructure_retries} "
            "additional attempts; successful trials and low scores are not retried."
        ),
        (
            "Successful trials and final failures count the selected final result. "
            "Paired report metrics include only final completed attempts from eligible "
            "A/B pairs and exclude failed-attempt overhead. Raw attempt evidence is "
            "retained separately; unfinished trials include trials not yet started."
        ),
        "",
        (
            "| Profile | Successful trials | Paired report trials | Extra retries | "
            "Failed attempts | Final failures | Unfinished trials |"
        ),
        "|---|---:|---:|---:|---:|---:|---:|",
    ]
    for profile, row in retry_counts(artifacts).items():
        lines.append(
            f"| {profile} | {row['successful']}/{row['planned']} | {row['paired']} | "
            f"{row['retries']} | {row['failed_attempts']} | {row['final_failures']} | "
            f"{row['unfinished']} |"
        )
    return "\n".join(lines) + "\n"


def _copy(source: Path, output: Path) -> None:
    if source.is_symlink():
        raise ValueError("compact reports cannot contain symlinks")
    if not source.exists():
        return
    if source.is_dir():
        for child in sorted(source.iterdir()):
            _copy(child, output / child.name)
    elif stat.S_ISREG(source.stat().st_mode):
        output.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, output)
    else:
        raise ValueError("compact reports require regular files")


def compact_report(evidence: Path, output: Path) -> None:
    """Copy only public metadata and reports from already secret-scanned evidence."""
    if evidence.is_symlink() or not evidence.is_dir():
        raise ValueError("scanned evidence must be a directory")
    source_root, output_root = evidence.resolve(), output.resolve()
    if output_root.is_relative_to(source_root) or source_root.is_relative_to(
        output_root
    ):
        raise ValueError("compact output must be separate from scanned evidence")
    if output.exists() or output.is_symlink():
        raise ValueError("compact output must be a new directory")
    output.mkdir(parents=True)
    try:
        for name in PUBLIC_FILES:
            _copy(evidence / name, output / name)
        runs = evidence / "runs"
        if runs.is_symlink():
            raise ValueError("compact reports cannot contain symlinks")
        for run in sorted(runs.iterdir()) if runs.is_dir() else []:
            if run.is_symlink():
                raise ValueError("compact reports cannot contain symlinks")
            if run.is_dir():
                _copy(run / "run.json", output / "runs" / run.name / "run.json")
                _copy(run / "report", output / "runs" / run.name / "report")
    except BaseException:
        shutil.rmtree(output)
        raise


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    retries = commands.add_parser("retries")
    retries.add_argument("--artifacts", type=Path, default=DEFAULT_ARTIFACTS_DIR)
    compact = commands.add_parser("compact")
    compact.add_argument("--evidence", type=Path, required=True)
    compact.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == "compact":
            compact_report(args.evidence, args.output)
        else:
            markdown = render_retry_summary(args.artifacts)
            if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
                with Path(summary).open("a", encoding="utf-8") as stream:
                    stream.write(markdown)
            else:
                print(markdown, end="")
    except (OSError, ValueError):
        # Diagnostic JSON and paths may contain secrets. Never echo error details.
        print("BrowseComp-Plus CI report export failed.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
