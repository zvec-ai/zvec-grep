"""Plan parallel case jobs and combine their raw results without re-judging."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import tempfile
from pathlib import Path

from .artifacts import read_json, sha256_file, utc_now, write_json
from .config import DEFAULT_ARTIFACTS_DIR, load_config
from .dataset import load_queries
from .evaluate import evaluation_complete, summarize_evaluation
from .index import _prepare_remote_authorization, index_is_ready
from .process import resolve_executable
from .profiles import model_base_url, server_environment
from .runner import (
    _randomized_profile_orders,
    _write_pair,
    load_suite,
    run_benchmark,
    select_case,
)


def restore_queries(artifacts: Path, plan_path: Path) -> Path:
    """Restore only validated query bytes; create_plan generates this run's plan."""
    source = plan_path.parent / "source" / "browsecomp_plus_decrypted.jsonl"
    if (
        plan_path.parent.is_symlink()
        or plan_path.is_symlink()
        or not plan_path.is_file()
        or source.parent.is_symlink()
        or source.is_symlink()
        or not source.is_file()
    ):
        raise ValueError("reuse plan and query set must be regular files")
    try:
        plan = read_json(plan_path)
        payload = source.read_bytes()
    except (OSError, ValueError):
        raise ValueError("reuse plan or query set could not be read") from None
    candidate = os.environ.get("BROWSE_CANDIDATE_COMMIT")
    if (
        not candidate
        or not isinstance(plan, dict)
        or plan.get("candidate_commit") != candidate
    ):
        raise ValueError("reuse plan candidate identity does not match")
    if hashlib.sha256(payload).hexdigest() != plan.get("query_set_sha256"):
        raise ValueError("reuse plan query set checksum does not match")

    destination = artifacts / "source" / source.name
    if (
        artifacts.is_symlink()
        or destination.parent.is_symlink()
        or destination.is_symlink()
        or destination.exists()
    ):
        raise ValueError("query restore requires an unused regular destination")
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            dir=destination.parent, prefix=".restore-queries-", delete=False
        ) as staging:
            temporary = Path(staging.name)
            staging.write(payload)
        # Validate the same bytes that were hashed and will be published, even if
        # the downloaded source changes after it was read. load_queries rejects
        # malformed JSON rows, unsafe IDs, and duplicate IDs.
        try:
            queries = load_queries(temporary)
        except (OSError, ValueError, TypeError):
            raise ValueError("reuse plan query set is invalid") from None
        if len(queries) != load_config().dataset.expected_queries:
            raise ValueError("reuse plan query set has the wrong query count")
        temporary.replace(destination)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
    return destination


def create_plan(
    artifacts: Path, suite: str, output: Path, *, execution: str = "parallel_cases"
) -> Path:
    config = load_config()
    query_path = artifacts / "source" / "browsecomp_plus_decrypted.jsonl"
    selection = load_suite(suite, load_queries(query_path))
    plan = {
        "suite": suite,
        "selection": selection.to_dict(),
        "query_ids": list(selection.query_ids),
        "profile_orders": _randomized_profile_orders(
            list(selection.query_ids), config.run.trials_per_case
        ),
        "configuration_sha256": sha256_file(config.path),
        "query_set_sha256": sha256_file(query_path),
        "candidate_commit": os.environ["BROWSE_CANDIDATE_COMMIT"],
        "harness_commit": os.environ["BROWSE_HARNESS_COMMIT"],
        "model_base_url": model_base_url(),
        "execution": execution,
    }
    write_json(output / "plan.json", plan)
    (output / "source").mkdir(exist_ok=True)
    shutil.copyfile(query_path, output / "source" / query_path.name)
    print("cases=" + json.dumps(plan["query_ids"]))
    return output / "plan.json"


def validate_plan(path: Path, artifacts: Path) -> dict:
    plan = read_json(path)
    config = load_config()
    query_path = artifacts / "source" / "browsecomp_plus_decrypted.jsonl"
    if (
        plan["configuration_sha256"] != sha256_file(config.path)
        or plan["query_set_sha256"] != sha256_file(query_path)
        or plan["candidate_commit"] != os.environ["BROWSE_CANDIDATE_COMMIT"]
        or plan["harness_commit"] != os.environ["BROWSE_HARNESS_COMMIT"]
        or plan["model_base_url"] != model_base_url()
    ):
        raise ValueError("CI plan identity changed")
    selection = load_suite(plan["suite"], load_queries(query_path))
    if plan["selection"] != selection.to_dict() or plan["query_ids"] != list(
        selection.query_ids
    ):
        raise ValueError("CI suite selection changed")
    for case in plan["query_ids"]:
        orders = plan["profile_orders"][case]
        if set(orders) != {
            str(i) for i in range(1, config.run.trials_per_case + 1)
        } or any(
            len(order) != 2 or set(order) != {"baseline", "zvec-grep"}
            for order in orders.values()
        ):
            raise ValueError("CI plan has invalid paired trial orders")
    return plan


def run_case(
    artifacts: Path,
    plan_path: Path,
    case: str,
    previous_evidence: Path | None = None,
    *,
    shared_runtime: dict | None = None,
) -> int:
    plan = validate_plan(plan_path, artifacts)
    if case not in plan["query_ids"]:
        raise ValueError("case is not part of the CI plan")
    config = load_config()
    zg = resolve_executable("zg")
    if zg is None or (shared_runtime is None and not index_is_ready(config, artifacts)):
        raise ValueError("prepared index is unavailable; workers never rebuild it")
    # Authorization belongs to this VM's fresh runtime, never to the cached snapshot.
    from .corpus import workspace_root

    _prepare_remote_authorization(
        config,
        zg,
        workspace_root(artifacts, "zvec-grep"),
        server_environment(config, artifacts),
    )
    orders = {
        query_id: {int(i): tuple(order) for i, order in trials.items()}
        for query_id, trials in plan["profile_orders"].items()
    }
    run_id = None
    if previous_evidence is not None:
        from .ci_resume import restore_case

        run_id = restore_case(
            artifacts, previous_evidence, plan_path, plan, case, config
        )
    root = run_benchmark(
        config,
        artifacts,
        suite=plan["suite"],
        case_id=case,
        planned_orders=orders,
        run_id=run_id,
        fresh_profiles=run_id is not None,
        **({"shared_runtime": shared_runtime} if shared_runtime is not None else {}),
    )
    return 0 if read_json(root / "run.json")["status"] == "completed" else 1


def select_shards(shards: Path, plan_path: Path, plan: dict) -> list[Path]:
    """Reuse earlier successful jobs on re-runs, without mixing evaluation plans."""
    from .ci_resume import evidence_roots, verify_evidence

    selected = {}
    for evidence in evidence_roots(shards):
        identity = read_json(evidence / "ci.json")
        if any(
            identity.get(key) != plan[key]
            for key in ("candidate_commit", "harness_commit")
        ) or identity.get("plan_sha256") != sha256_file(plan_path):
            continue
        verify_evidence(evidence)
        case = identity.get("case_id")
        if case not in plan["query_ids"]:
            raise ValueError("unexpected CI case")
        if (
            os.environ.get("GITHUB_RUN_ID")
            and identity.get("workflow_run_id") != os.environ["GITHUB_RUN_ID"]
        ):
            raise ValueError("case artifact belongs to a different workflow run")
        attempt = int(identity["workflow_run_attempt"])
        if case in selected and attempt == selected[case][0]:
            raise ValueError("duplicate CI case artifact for the same attempt")
        if case not in selected or attempt > selected[case][0]:
            selected[case] = (attempt, evidence)
    return [entry[1] for entry in selected.values()]


def aggregate(artifacts: Path, plan_path: Path, shards: Path) -> Path:
    # Only the small query set is needed here, not the multi-GB index snapshot.
    source = artifacts / "source"
    source.mkdir(parents=True, exist_ok=True)
    query_source = plan_path.parent / "source" / "browsecomp_plus_decrypted.jsonl"
    query_target = source / "browsecomp_plus_decrypted.jsonl"
    if query_source.resolve() != query_target.resolve():
        shutil.copyfile(query_source, query_target)
    plan = validate_plan(plan_path, artifacts)
    config = load_config()
    selection = load_suite(
        plan["suite"], load_queries(source / "browsecomp_plus_decrypted.jsonl")
    )
    root = artifacts / "runs" / "ci-aggregate"
    root.mkdir(parents=True, exist_ok=False)
    seen = set()
    metadata = None
    errors = []
    runtime_setups = []
    for evidence in select_shards(shards, plan_path, plan):
        identity = read_json(evidence / "ci.json")
        if any(
            identity.get(key) != plan[key]
            for key in ("candidate_commit", "harness_commit")
        ) or identity.get("plan_sha256") != sha256_file(plan_path):
            raise ValueError("shard identity does not match the CI plan")
        case = identity.get("case_id")
        if case not in plan["query_ids"] or case in seen:
            raise ValueError("unexpected or duplicate CI case")
        seen.add(case)
        runs = list((evidence / "runs").glob("*/run.json"))
        if not runs:
            errors.append(f"case {case}: no run was created")
            continue
        if len(runs) != 1:
            raise ValueError("expected exactly one run per CI case")
        run = runs[0].parent
        current = read_json(runs[0])
        if (
            current["query_ids"] != [case]
            or current["suite"] != plan["suite"]
            or current["suite_selection"] != select_case(selection, case).to_dict()
            or current["trials_per_case"] != config.run.trials_per_case
            or current["model"] != config.run.model
            or current["reasoning_effort"] != config.run.reasoning_effort
            or current["profile_orders"] != {case: plan["profile_orders"][case]}
            or current["environment"]["model_base_url"] != plan["model_base_url"]
        ):
            raise ValueError("shard protocol does not match the CI plan")
        if metadata is None:
            metadata = current.copy()
        elif any(
            current[key] != metadata[key]
            for key in ("corpus_fingerprint", "index_fingerprint", "index_statistics")
        ):
            raise ValueError("shards used different prepared corpora or indexes")
        if any(
            current["environment"][key] != metadata["environment"][key]
            for key in ("opencode_version", "zg_version", "machine")
        ):
            raise ValueError(
                "shards used different executable versions or architectures"
            )
        for setup in current["runtime_setups"]:
            shared_id = setup.get("shared_runtime_id")
            previous = next(
                (
                    item
                    for item in runtime_setups
                    if shared_id and item.get("shared_runtime_id") == shared_id
                ),
                None,
            )
            if previous is not None:
                if setup != previous:
                    raise ValueError(
                        "shared runtime measurements changed between cases"
                    )
            else:
                runtime_setups.append(setup)
        case_root = run / "cases" / case
        if case_root.exists():
            shutil.copytree(case_root, root / "cases" / case)
        for profile in ("baseline", "zvec-grep"):
            relative = Path("evaluation/results") / profile / case
            if (run / relative).exists():
                shutil.copytree(run / relative, root / relative)
        relative = Path("evaluation/usage-audit/results") / case
        if (run / relative).exists():
            shutil.copytree(run / relative, root / relative)
        # Recreate absolute result references for this aggregate workspace.
        if (
            not _write_pair(root, case, config.run.trials_per_case)
            or current["status"] != "completed"
        ):
            errors.append(f"case {case}: incomplete or failed trials")
        write_json(root / "shards" / f"{case}.json", current)
    missing = sorted(set(plan["query_ids"]) - seen)
    errors.extend(f"case {case}: missing artifact" for case in missing)
    if metadata is None:
        raise ValueError("no case results are available to aggregate")
    metadata.update(
        run_id=root.name,
        query_ids=plan["query_ids"],
        suite_selection=plan["selection"],
        profile_orders=plan["profile_orders"],
        generated_at=utc_now(),
        created_at=utc_now(),
        finished_at=utc_now(),
        execution=plan.get("execution", "parallel_cases"),
        aggregate_only=True,
        shard_errors=errors,
        runtime_setups=runtime_setups,
        status="partial" if errors else "completed",
    )
    # This is a derived report, not a resumable execution; keep source protocols in shards/.
    for key in (
        "profiles_manifest",
        "profiles_fingerprint",
        "protocol",
        "protocol_fingerprint",
        "error",
    ):
        metadata.pop(key, None)
    write_json(root / "run.json", metadata)
    report = summarize_evaluation(
        root, opencode_version=metadata["environment"]["opencode_version"]
    )
    summary = read_json(report / "summary.json")
    if summary["cases"]["completed"] != len(
        plan["query_ids"]
    ) or not evaluation_complete(root):
        errors.append("not all planned trials and judgements completed")
    metadata["completed_cases"] = summary["cases"]["completed"]
    metadata["status"] = "partial" if errors else "completed"
    write_json(root / "run.json", metadata)
    completeness = {
        "complete": not errors,
        "errors": errors,
        "planned_cases": plan["query_ids"],
    }
    write_json(report / "ci-completeness.json", completeness)
    from .ci_resume import execution_costs

    summary["ci"] = {
        "execution": plan.get("execution", "parallel_cases"),
        **completeness,
        "agent_attempt_costs": execution_costs(root),
    }
    write_json(report / "summary.json", summary)
    markdown = report / "summary.md"
    status = "COMPLETE" if not errors else "INCOMPLETE"
    notes = (
        f"**CI result: {status}.** Execution: {metadata['execution']}; paired trials "
        f"per case: {config.run.trials_per_case}, executed in the planned A/B order. "
        "Runtime preparation totals sum worker "
        "time, not elapsed workflow time. Index build time describes the original "
        "snapshot build. The environment table is one worker's environment; "
        "all worker metadata is in `shards/`. Agent metrics use selected completed "
        "pairs; `ci.agent_attempt_costs` also includes failed and earlier manual "
        "attempts, with missing usage/time explicitly marked.\n\n"
    )
    if errors:
        notes += "\n".join(f"- {error}" for error in errors) + "\n\n"
    markdown.write_text(notes + markdown.read_text(encoding="utf-8"), encoding="utf-8")
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    plan = sub.add_parser("plan")
    plan.add_argument("--suite", choices=("iteration", "ci", "smoke"), required=True)
    plan.add_argument("--output", type=Path, required=True)
    plan.add_argument(
        "--execution",
        choices=("parallel_cases", "shared_job"),
        default="parallel_cases",
    )
    restore = sub.add_parser("restore-queries")
    restore.add_argument("--plan", type=Path, required=True)
    run = sub.add_parser("run")
    run.add_argument("--plan", type=Path, required=True)
    run.add_argument("--case", required=True)
    run.add_argument("--previous-evidence", type=Path)
    collect = sub.add_parser("aggregate")
    collect.add_argument("--plan", type=Path, required=True)
    collect.add_argument("--shards", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "plan":
        create_plan(
            DEFAULT_ARTIFACTS_DIR, args.suite, args.output, execution=args.execution
        )
        return 0
    if args.command == "restore-queries":
        restore_queries(DEFAULT_ARTIFACTS_DIR, args.plan)
        return 0
    if args.command == "run":
        return run_case(
            DEFAULT_ARTIFACTS_DIR, args.plan, args.case, args.previous_evidence
        )
    report = aggregate(DEFAULT_ARTIFACTS_DIR, args.plan, args.shards)
    return 0 if read_json(report / "ci-completeness.json")["complete"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
