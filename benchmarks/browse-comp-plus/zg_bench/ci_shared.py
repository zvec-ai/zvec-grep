"""Run isolated case processes against one prepared corpus and search daemon."""

from __future__ import annotations

import argparse
import os
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path
from time import sleep

from .artifacts import read_json, sha256_file, utc_now, write_json
from .ci import configure_auth, export_evidence
from .ci_parallel import run_case, validate_plan
from .ci_reports import compact_report
from .ci_resume import evidence_roots, verify_evidence
from .config import DEFAULT_ARTIFACTS_DIR, load_config
from .console import Console
from .evaluate import evaluate_run, evaluation_complete
from .index import index_is_ready
from .profiles import ensure_server, prepare_search_runtime, server_url, stop_server
from .runner import RunTerminated, _cleanup_on_termination


def group_previous(previous: Path, output: Path, plan_path: Path, plan: dict) -> None:
    """Give each case only its own verified evidence from earlier attempts."""
    for root in evidence_roots(previous):
        verify_evidence(root)
        identity = read_json(root / "ci.json")
        case = identity.get("case_id")
        if (
            case not in plan["query_ids"]
            or identity.get("workflow_run_id") != os.environ["GITHUB_RUN_ID"]
            or identity.get("plan_sha256") != sha256_file(plan_path)
            or identity.get("candidate_commit") != plan["candidate_commit"]
            or identity.get("harness_commit") != plan["harness_commit"]
        ):
            raise ValueError("previous bundle does not match the current CI plan")
        attempt = int(identity["workflow_run_attempt"])
        if not 1 <= attempt < int(os.environ["GITHUB_RUN_ATTEMPT"]):
            raise ValueError("previous bundle has an invalid workflow attempt")
        destination = output / case / f"attempt-{attempt}"
        if destination.exists():
            raise ValueError("duplicate previous case evidence")
        shutil.copytree(root, destination)


def case_run(artifacts: Path, case: str) -> Path | None:
    matches = [
        path.parent
        for path in (artifacts / "runs").glob("*/run.json")
        if read_json(path).get("query_ids") == [case]
    ]
    if len(matches) > 1:
        raise ValueError("expected one isolated execution per case")
    return matches[0] if matches else None


def validate_runtime(artifacts: Path, runtime: dict) -> None:
    if (
        runtime.get("shared_runtime_id")
        != f"{os.environ['GITHUB_RUN_ID']}-{os.environ['GITHUB_RUN_ATTEMPT']}"
        or runtime.get("root") != str((artifacts / "workspaces/b").resolve())
        or runtime.get("server_url") != server_url(load_config())
    ):
        raise ValueError("shared search runtime identity changed")


def stage_case(
    artifacts: Path,
    case: str,
    plan_path: Path,
    evidence: Path,
    reports: Path,
    candidate_manifest: Path,
) -> None:
    """Also called by the parent after an abruptly terminated worker."""
    home = artifacts / "runtime/case-homes" / case
    if (evidence / case).exists():
        verify_evidence(evidence / case)
    else:
        export_evidence(
            artifacts,
            evidence / case,
            home,
            candidate_manifest,
            plan_path,
            case_id=case,
        )
    if not (reports / case).exists():
        compact_report(evidence / case, reports / case)


def run_worker(
    artifacts: Path,
    plan_path: Path,
    case: str,
    previous: Path,
    evidence: Path,
    reports: Path,
    candidate_manifest: Path,
) -> int:
    plan = validate_plan(plan_path, artifacts)
    if plan.get("execution") != "shared_job" or case not in plan["query_ids"]:
        raise ValueError("case is not in a shared-job plan")
    home = artifacts / "runtime/case-homes" / case
    scratch = artifacts / "runtime/case-tmp" / case
    scratch.mkdir(parents=True, exist_ok=False)
    os.environ.update(
        BROWSE_CASE=case, OPENCODE_HOME=str(home), TMPDIR=str(scratch.resolve())
    )
    runtime = read_json(artifacts / "state/runtime.json")
    validate_runtime(artifacts, runtime)
    code = 1
    interrupted = None
    try:
        configure_auth(home)
        code = run_case(artifacts, plan_path, case, previous, shared_runtime=runtime)
    except BaseException as error:  # noqa: BLE001 -- always preserve raw failure/cancellation evidence
        # Provider output and exception text may contain credentials.
        print(f"Case {case}: execution stopped ({type(error).__name__}).", flush=True)
        if isinstance(error, (KeyboardInterrupt, RunTerminated)):
            interrupted = error
    finally:
        try:
            root = case_run(artifacts, case)
            if root is not None and interrupted is None:
                try:
                    evaluate_run(artifacts, root, console=Console())
                    if not evaluation_complete(root):
                        code = 1
                except BaseException as error:  # noqa: BLE001 -- always preserve raw failure/cancellation evidence
                    print(
                        f"Case {case}: judging stopped ({type(error).__name__}).",
                        flush=True,
                    )
                    code = 1
                    if isinstance(error, (KeyboardInterrupt, RunTerminated)):
                        interrupted = error
            stage_case(
                artifacts, case, plan_path, evidence, reports, candidate_manifest
            )
        finally:
            shutil.rmtree(home, ignore_errors=True)
    print(
        f"Case {case}: {'complete' if code == 0 else 'failed'}; evidence saved.",
        flush=True,
    )
    if interrupted is not None:
        raise interrupted
    return code


def stop_workers(active: dict[str, subprocess.Popen]) -> None:
    for process in active.values():
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
    deadline = time.monotonic() + 20
    for process in active.values():
        try:
            process.wait(timeout=max(0.1, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()


def run_shared(
    artifacts: Path,
    plan_path: Path,
    parallel_cases: int,
    previous: Path,
    evidence: Path,
    reports: Path,
    candidate_manifest: Path,
) -> int:
    if parallel_cases not in (1, 2, 5):
        raise ValueError("case concurrency must be 1, 2, or 5")
    plan = validate_plan(plan_path, artifacts)
    if plan.get("execution") != "shared_job":
        raise ValueError("expected a shared-job evaluation plan")
    config = load_config()
    if not index_is_ready(config, artifacts):
        raise ValueError("prepared index is unavailable; shared jobs never rebuild it")
    grouped = artifacts / "runtime/previous-cases"
    group_previous(previous, grouped, plan_path, plan)
    pending = list(plan["query_ids"])
    active: dict[str, subprocess.Popen] = {}
    outcomes = {}
    started = time.monotonic()
    started_at = utc_now()
    with _cleanup_on_termination():
        try:
            ensure_server(config, artifacts, restart=True)
            server_seconds = time.monotonic() - started
            runtime = prepare_search_runtime(config, artifacts)
            runtime.update(
                shared_runtime_id=f"{os.environ['GITHUB_RUN_ID']}-{os.environ['GITHUB_RUN_ATTEMPT']}",
                started_at=started_at,
                finished_at=utc_now(),
                server_start_wall_seconds=server_seconds,
                profile_preparation_wall_seconds=0.0,
                profile_install_wall_seconds=0.0,
                total_wall_seconds=time.monotonic() - started,
            )
            write_json(artifacts / "state/runtime.json", runtime)
            heartbeat = time.monotonic() + 60
            while pending or active:
                while pending and len(active) < parallel_cases:
                    case = pending.pop(0)
                    command = [
                        sys.executable,
                        "-m",
                        "zg_bench.ci_shared",
                        "case",
                        "--artifacts",
                        str(artifacts),
                        "--plan",
                        str(plan_path),
                        "--case",
                        case,
                        "--previous-evidence",
                        str(grouped / case),
                        "--evidence",
                        str(evidence),
                        "--reports",
                        str(reports),
                        "--candidate-manifest",
                        str(candidate_manifest),
                    ]
                    active[case] = subprocess.Popen(command, start_new_session=True)
                    print(f"Case {case}: started; shared index and daemon.", flush=True)
                for case, process in list(active.items()):
                    code = process.poll()
                    if code is not None:
                        outcomes[case] = code
                        del active[case]
                if time.monotonic() >= heartbeat:
                    print(
                        f"Shared job: {len(outcomes)}/{len(plan['query_ids'])} finished; active {', '.join(active)}.",
                        flush=True,
                    )
                    heartbeat = time.monotonic() + 60
                if active:
                    sleep(0.2)
        finally:
            try:
                stop_workers(active)
                for case in plan["query_ids"]:
                    try:
                        stage_case(
                            artifacts,
                            case,
                            plan_path,
                            evidence,
                            reports,
                            candidate_manifest,
                        )
                    except (OSError, ValueError):
                        outcomes[case] = 1
                        print(f"Case {case}: evidence export failed.", flush=True)
                    finally:
                        shutil.rmtree(
                            artifacts / "runtime/case-homes" / case, ignore_errors=True
                        )
            finally:
                stop_server(config, artifacts)
            write_json(
                artifacts / "state/shared-execution.json",
                {
                    "execution": "shared_job",
                    "parallel_cases": parallel_cases,
                    "cases": outcomes,
                    "planned_cases": plan["query_ids"],
                    "finished_at": utc_now(),
                },
            )
    return (
        0
        if len(outcomes) == len(plan["query_ids"]) and not any(outcomes.values())
        else 1
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("run", "case"))
    parser.add_argument("--artifacts", type=Path, default=DEFAULT_ARTIFACTS_DIR)
    parser.add_argument("--plan", type=Path, required=True)
    parser.add_argument("--parallel-cases", type=int, choices=(1, 2, 5), default=2)
    parser.add_argument("--case")
    parser.add_argument("--previous-evidence", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--reports", type=Path, required=True)
    parser.add_argument("--candidate-manifest", type=Path, required=True)
    args = parser.parse_args()
    common = (args.artifacts, args.plan)
    tail = (
        args.previous_evidence,
        args.evidence,
        args.reports,
        args.candidate_manifest,
    )
    if args.command == "case":
        if not args.case:
            parser.error("case requires --case")
        with _cleanup_on_termination():
            return run_worker(*common, args.case, *tail)
    return run_shared(*common, args.parallel_cases, *tail)


if __name__ == "__main__":
    raise SystemExit(main())
