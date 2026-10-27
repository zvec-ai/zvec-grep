"""Verified raw evidence import for an explicit GitHub Actions job re-run.

A new workflow attempt replenishes the per-trial automatic retry budget exactly
once. Completed answers are immutable, independently of their judged quality.
"""

from __future__ import annotations

import os
import re
import shutil
import stat
from pathlib import Path
from typing import Any

from .artifacts import next_attempt_number, read_json, sha256_file, utc_now, write_json
from .config import BenchmarkConfig
from .models import PROFILES

MANIFEST = "evidence-manifest.json"


def evidence_files(root: Path) -> dict[str, str]:
    if root.is_symlink() or not root.is_dir():
        raise ValueError("raw evidence must be a regular directory")
    files = {}
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise ValueError("raw evidence contains a symlink")
        if path.is_dir():
            continue
        if not stat.S_ISREG(path.stat().st_mode):
            raise ValueError("raw evidence contains a non-regular file")
        if path == root / MANIFEST:
            continue
        files[path.relative_to(root).as_posix()] = sha256_file(path)
    return files


def seal_evidence(root: Path) -> None:
    write_json(root / MANIFEST, {"version": 1, "files": evidence_files(root)})


def verify_evidence(root: Path) -> None:
    manifest = root / MANIFEST
    if manifest.is_symlink() or not manifest.is_file():
        raise ValueError("raw evidence checksum manifest is missing")
    recorded = read_json(manifest)
    if recorded != {"version": 1, "files": evidence_files(root)}:
        raise ValueError("raw evidence integrity check failed")


def retry_start(selected_path: Path) -> int:
    budget = selected_path.parent / "ci-retry-budget.json"
    if not budget.is_file():
        return 1
    identity = read_json(budget)
    if (
        identity.get("workflow_run_id") != os.environ.get("GITHUB_RUN_ID")
        or identity.get("workflow_run_attempt") != os.environ.get("GITHUB_RUN_ATTEMPT")
        or not isinstance(identity.get("first_attempt"), int)
        or identity["first_attempt"] < 1
    ):
        raise ValueError("manual retry budget belongs to a different workflow attempt")
    return identity["first_attempt"]


def _validate_trials(run: Path, case: str, trials: int) -> None:
    for path in (run / "cases").glob(
        "*/*/trials/trial-*/attempts/attempt-*/result.json"
    ):
        result = read_json(path)
        profile = path.parents[4].name
        index = path.parents[2].name
        attempt = path.parent.name
        if (
            path.parents[5].name != case
            or profile not in PROFILES
            or index not in {f"trial-{n:03d}" for n in range(1, trials + 1)}
            or not re.fullmatch(r"attempt-\d{3,}", attempt)
            or str(result.get("query_id")) != case
            or result.get("profile") != profile
            or result.get("trial_index") != int(index.removeprefix("trial-"))
            or result.get("attempt") != int(attempt.removeprefix("attempt-"))
            or result.get("status")
            not in {"completed", "failed", "infrastructure_failed"}
        ):
            raise ValueError("raw trial identity is invalid")
        # Retain original result bytes; the current runtime protocol validates
        # the fixed runner workspace before any imported trial is used.
        for name in ("events.jsonl", "stderr.log", "response.md"):
            if not (path.parent / name).is_file():
                raise ValueError("raw trial evidence is incomplete")
        if result["status"] == "completed" and (
            result.get("exit_code") != 0
            or result.get("infrastructure_failure") is not False
            or result.get("trace", {}).get("turn_completed") is not True
            or not result.get("trace", {}).get("final_response", "").strip()
            or result.get("trace", {}).get("errors")
        ):
            raise ValueError("raw completed trial is invalid")
    for selected in (run / "cases").glob("*/*/trials/trial-*/result.json"):
        result = read_json(selected)
        attempt = (
            selected.parent
            / "attempts"
            / f"attempt-{int(result['attempt']):03d}"
            / "result.json"
        )
        if not attempt.is_file() or selected.read_bytes() != attempt.read_bytes():
            raise ValueError("selected result does not match its raw attempt")


def restore_case(
    artifacts: Path,
    previous: Path,
    plan_path: Path,
    plan: dict[str, Any],
    case: str,
    config: BenchmarkConfig,
) -> str | None:
    """Import the newest cumulative raw artifact, rejecting mixed/edited runs."""
    workflow_id = os.environ.get("GITHUB_RUN_ID")
    current_attempt = int(os.environ.get("GITHUB_RUN_ATTEMPT", "0"))
    if not workflow_id or current_attempt < 1:
        raise ValueError("raw evidence reuse requires a GitHub workflow identity")
    if not previous.exists():
        return None
    if previous.is_symlink() or not previous.is_dir():
        raise ValueError("previous evidence must be a regular directory")
    sources = []
    attempts = set()
    for evidence in sorted(previous.iterdir()):
        verify_evidence(evidence)
        identity = read_json(evidence / "ci.json")
        attempt = int(identity.get("workflow_run_attempt", "0"))
        if (
            identity.get("workflow_run_id") != workflow_id
            or identity.get("case_id") != case
            or any(
                identity.get(key) != plan[key]
                for key in ("candidate_commit", "harness_commit")
            )
            or not 1 <= attempt < current_attempt
            or sha256_file(evidence / "benchmark.toml") != plan["configuration_sha256"]
        ):
            raise ValueError(
                "previous case evidence does not match the current CI plan"
            )
        if attempt in attempts:
            raise ValueError("duplicate raw CI artifact for the same attempt")
        attempts.add(attempt)
        prior_plan = evidence / "plan.json"
        prior_runs = evidence / "runs"
        # A cancelled download can export diagnostics before plan-download.
        # Only diagnostics with no execution may start under the current plan.
        if (
            identity.get("plan_sha256") in (None, "")
            and not prior_plan.exists()
            and (
                not prior_runs.exists()
                or (prior_runs.is_dir() and not any(prior_runs.iterdir()))
            )
        ):
            continue
        if (
            identity.get("plan_sha256") != sha256_file(plan_path)
            or not prior_plan.is_file()
            or sha256_file(prior_plan) != sha256_file(plan_path)
        ):
            raise ValueError(
                "previous case evidence does not match the current CI plan"
            )
        runs = list((evidence / "runs").glob("*/run.json"))
        if len(runs) > 1:
            raise ValueError("expected at most one run per raw case artifact")
        if not runs:
            continue  # Setup failed before executing any trials.
        run = runs[0].parent
        metadata = read_json(runs[0])
        protocol = metadata.get("protocol", {})
        from .runner import _protocol_fingerprint

        if (
            not re.fullmatch(r"[A-Za-z0-9_-]+", run.name)
            or metadata.get("run_id") != run.name
            or metadata.get("query_ids") != [case]
            or metadata.get("suite") != plan["suite"]
            or metadata.get("profile_orders") != {case: plan["profile_orders"][case]}
            or metadata.get("trials_per_case") != config.run.trials_per_case
            or metadata.get("protocol_fingerprint") != _protocol_fingerprint(protocol)
            or protocol.get("configuration_sha256") != plan["configuration_sha256"]
            or protocol.get("query_set_sha256") != plan["query_set_sha256"]
            or protocol.get("model_base_url") != plan["model_base_url"]
            or protocol.get("profiles_fingerprint")
            != metadata.get("profiles_fingerprint")
        ):
            raise ValueError("previous case execution protocol is invalid")
        _validate_trials(run, case, config.run.trials_per_case)
        sources.append((attempt, evidence, run, metadata))
    if not sources:
        return None
    sources.sort(key=lambda row: row[0])
    _, _, latest, metadata = sources[-1]
    # Re-runs are cumulative. Never silently discard a completed answer or any
    # prior raw attempt when several artifact generations were downloaded.
    for _, _, older, old_metadata in sources[:-1]:
        if (
            older.name != latest.name
            or old_metadata["protocol"] != metadata["protocol"]
        ):
            raise ValueError("raw artifacts contain different executions")
        for old in (older / "cases").rglob("*"):
            if not old.is_file() or old.name in {"ci-retry-budget.json", "pair.json"}:
                continue
            relative = old.relative_to(older)
            if "attempts" not in relative.parts and old.name == "result.json":
                if read_json(old).get("status") != "completed":
                    continue
            elif "attempts" not in relative.parts:
                continue
            new = latest / relative
            if not new.is_file() or sha256_file(old) != sha256_file(new):
                raise ValueError(
                    "raw artifact discarded or changed prior trial evidence"
                )
    destination = artifacts / "runs" / latest.name
    if destination.exists():
        raise ValueError("raw case restore requires an unused run destination")
    destination.mkdir(parents=True)
    try:
        for name in ("run.json", "cases", "evaluation", "resume"):
            source = latest / name
            if source.is_dir():
                shutil.copytree(source, destination / name)
            elif source.is_file():
                shutil.copyfile(source, destination / name)
        origins = []
        for attempt, evidence, run, source_metadata in sources:
            origin = {
                "workflow_run_id": workflow_id,
                "workflow_run_attempt": str(attempt),
                "artifact_directory": evidence.name,
                "manifest_sha256": sha256_file(evidence / MANIFEST),
                "run_id": run.name,
            }
            origins.append(origin)
            write_json(
                destination / "resume" / "sources" / f"workflow-attempt-{attempt}.json",
                {"origin": origin, "run": source_metadata},
            )
        resumed_trials = []
        for profile in PROFILES:
            for index in range(1, config.run.trials_per_case + 1):
                trial = (
                    destination
                    / "cases"
                    / case
                    / profile
                    / "trials"
                    / f"trial-{index:03d}"
                )
                selected = trial / "result.json"
                raw = sorted((trial / "attempts").glob("attempt-*"))
                for old in raw:
                    if not old.is_dir() or not re.fullmatch(
                        r"attempt-\d{3,}", old.name
                    ):
                        raise ValueError("invalid raw attempt directory")
                    if not (old / "origin.json").exists():
                        write_json(old / "origin.json", origins[-1])
                # A cancellation can occur after result.json was written inside
                # the attempt but before selected result.json was published.
                completed = [
                    path / "result.json"
                    for path in raw
                    if (path / "result.json").is_file()
                    and read_json(path / "result.json").get("status") == "completed"
                ]
                if len(completed) > 1:
                    raise ValueError("multiple completed attempts for one trial")
                if completed:
                    if (
                        selected.is_file()
                        and read_json(selected).get("status") == "completed"
                        and selected.read_bytes() != completed[0].read_bytes()
                    ):
                        raise ValueError("selected completed answer changed")
                    selected.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(completed[0], selected)
                reused = (
                    selected.is_file()
                    and read_json(selected).get("status") == "completed"
                )
                write_json(
                    trial / "ci-retry-budget.json",
                    {
                        "workflow_run_id": workflow_id,
                        "workflow_run_attempt": str(current_attempt),
                        "first_attempt": next_attempt_number(trial / "attempts"),
                    },
                )
                resumed_trials.append(
                    {
                        "query_id": case,
                        "profile": profile,
                        "trial_index": index,
                        "reused_completed": reused,
                        "prior_attempts": len(raw),
                        "prior_attempts_without_result": sum(
                            not (path / "result.json").is_file() for path in raw
                        ),
                    }
                )
        history = metadata.setdefault("ci_resume_history", [])
        if any(
            row.get("workflow_run_attempt") == str(current_attempt) for row in history
        ):
            raise ValueError("manual workflow attempt was already imported")
        history.append(
            {
                "workflow_run_id": workflow_id,
                "workflow_run_attempt": str(current_attempt),
                "restored_at": utc_now(),
                "sources": origins,
                "trials": resumed_trials,
                "automatic_retries": config.run.infrastructure_retries,
            }
        )
        write_json(destination / "run.json", metadata)
    except BaseException:
        shutil.rmtree(destination)
        raise
    return destination.name


def execution_costs(run: Path) -> dict[str, Any]:
    """Account for all agent attempts, not only the final selected answers.

    A killed process can leave no usage or wall-time record. Missing values are
    explicitly counted; known totals are lower bounds, never zero-cost claims.
    """
    profiles = {}
    for profile in PROFILES:
        rows = []
        for attempt in sorted(
            run.glob(f"cases/*/{profile}/trials/trial-*/attempts/attempt-*")
        ):
            result_path = attempt / "result.json"
            result = read_json(result_path) if result_path.is_file() else {}
            origin_path = attempt / "origin.json"
            origin = read_json(origin_path) if origin_path.is_file() else {}
            usage = result.get("trace", {}).get("usage")
            if usage is None and (attempt / "usage.json").is_file():
                usage = read_json(attempt / "usage.json")
            rows.append(
                {
                    "query_id": attempt.parents[4].name,
                    "trial": attempt.parents[1].name,
                    "attempt": attempt.name,
                    "workflow_run_attempt": origin.get("workflow_run_attempt"),
                    "status": result.get("status", "incomplete"),
                    "wall_seconds": result.get("wall_seconds"),
                    "total_tokens": usage.get("input_tokens", 0)
                    + usage.get("output_tokens", 0)
                    if usage
                    else None,
                }
            )
        profiles[profile] = {
            "attempts": len(rows),
            "failed_attempts": sum(
                row["status"] in {"failed", "infrastructure_failed"} for row in rows
            ),
            "incomplete_attempts": sum(row["status"] == "incomplete" for row in rows),
            "known_wall_seconds": sum(
                row["wall_seconds"] for row in rows if row["wall_seconds"] is not None
            ),
            "wall_seconds_unavailable": sum(
                row["wall_seconds"] is None for row in rows
            ),
            "known_total_tokens": sum(
                row["total_tokens"] for row in rows if row["total_tokens"] is not None
            ),
            "usage_unavailable": sum(row["total_tokens"] is None for row in rows),
            "rows": rows,
        }
    return {
        "scope": "all agent attempts including previous manual workflow attempts",
        "profiles": profiles,
    }
