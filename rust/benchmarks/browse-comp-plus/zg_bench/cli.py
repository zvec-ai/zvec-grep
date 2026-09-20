from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
from pathlib import Path

from .artifacts import find_run, read_json
from .console import Console
from .config import DEFAULT_ARTIFACTS_DIR, load_config
from .corpus import materialize, prepared_corpus, workspace_root
from .dataset import fetch, prepared_dataset
from .doctor import format_report, run_doctor
from .evaluate import evaluate, evaluation_complete
from .index import build_index, index_is_ready, prepared_index
from .report import generate_report
from .runner import RunTerminated, resume_benchmark, run_benchmark


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="zg-bench",
        description="Native Codex A/B benchmark on BrowseComp-Plus.",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    doctor = subparsers.add_parser("doctor", help="validate the local environment")
    doctor.add_argument("--codex-bin", default="codex")
    doctor.add_argument("--json", action="store_true")

    prepare = subparsers.add_parser(
        "prepare", help="prepare benchmark data and the reusable index"
    )
    prepare.add_argument("--hf-token")
    prepare.add_argument(
        "--yes", action="store_true", help="confirm index build or rebuild"
    )

    run = subparsers.add_parser("run", help="run a paired suite")
    run.add_argument(
        "--suite",
        default="study",
        help="suite name or .txt path (default: study)",
    )
    run.add_argument("--codex-bin", default="codex")

    resume = subparsers.add_parser("resume", help="resume an interrupted run")
    resume.add_argument("run_id")
    resume.add_argument("--codex-bin", default="codex")

    status = subparsers.add_parser("status", help="show paired run progress")
    status.add_argument("run_id", nargs="?")

    inspect = subparsers.add_parser(
        "inspect", help="print persisted trials for one case and profile"
    )
    inspect.add_argument("run_id")
    inspect.add_argument("--case", required=True)
    inspect.add_argument("--profile", choices=("baseline", "zvec-grep"), required=True)
    inspect.add_argument("--events", action="store_true")

    evaluate_parser = subparsers.add_parser(
        "evaluate", help="judge completed runs and generate reports"
    )
    evaluate_parser.add_argument("run_id", nargs="?")
    evaluate_parser.add_argument("--codex-bin", default="codex")

    report = subparsers.add_parser(
        "report", help="generate a run report"
    )
    report.add_argument("run_id", nargs="?")

    clean = subparsers.add_parser(
        "clean", help="delete runs and reports while preserving prepared data"
    )
    clean.add_argument("--yes", action="store_true", help="skip confirmation")
    return parser


def _latest_run(artifacts: Path) -> Path | None:
    runs = artifacts / "runs"
    candidates = (
        [
            path
            for path in runs.iterdir()
            if path.is_dir() and (path / "run.json").is_file()
        ]
        if runs.is_dir()
        else []
    )
    return (
        max(
            candidates,
            key=lambda path: str(
                json.loads((path / "run.json").read_text(encoding="utf-8"))[
                    "created_at"
                ]
            ),
        )
        if candidates
        else None
    )


def _selected_run(artifacts: Path, run_id: str | None) -> Path:
    if run_id:
        return find_run(artifacts, run_id)
    latest = _latest_run(artifacts)
    if latest is None:
        raise RuntimeError("no benchmark runs found")
    return latest


def _status(artifacts: Path, run_id: str | None) -> int:
    root = _selected_run(artifacts, run_id)
    metadata = json.loads((root / "run.json").read_text(encoding="utf-8"))
    profiles = {"baseline": {}, "zvec-grep": {}}
    persisted_cases = 0
    completed_cases = 0
    persisted_trials = 0
    completed_trials = 0
    for query_id in metadata["query_ids"]:
        case = root / "cases" / str(query_id)
        pair_path = case / "pair.json"
        if pair_path.is_file():
            persisted_cases += 1
            pair = json.loads(pair_path.read_text(encoding="utf-8"))
            if pair["eligible"] is True:
                completed_cases += 1
            persisted_trials += int(pair["persisted_trials"])
            completed_trials += int(pair["eligible_trials"])
        for trial_index in range(1, int(metadata["trials_per_case"]) + 1):
            for profile in profiles:
                path = (
                    case
                    / profile
                    / "trials"
                    / f"trial-{trial_index:03d}"
                    / "result.json"
                )
                if path.is_file():
                    status = json.loads(path.read_text(encoding="utf-8"))["status"]
                    profiles[profile][status] = profiles[profile].get(status, 0) + 1
    print(f"Run: {metadata['run_id']}")
    print(f"Completed cases: {completed_cases} / {len(metadata['query_ids'])}")
    if persisted_cases != completed_cases:
        print(f"Persisted case records: {persisted_cases}")
    planned_trials = len(metadata["query_ids"]) * int(metadata["trials_per_case"])
    print(f"Completed trials: {completed_trials} / {planned_trials}")
    if persisted_trials != completed_trials:
        print(f"Persisted trial records: {persisted_trials}")
    for profile, counts in profiles.items():
        rendered = (
            ", ".join(f"{name}={count}" for name, count in sorted(counts.items()))
            or "none"
        )
        print(f"{profile}: {rendered}")
    return 0


def _clean(artifacts: Path, *, confirmed: bool) -> int:
    runs = artifacts / "runs"
    run_count = (
        sum(path.is_dir() for path in runs.iterdir()) if runs.is_dir() else 0
    )
    if run_count == 0:
        console = Console()
        console.heading("BrowseComp-Plus clean")
        console.success("No runs to remove")
        return 0

    console = Console()
    console.heading("BrowseComp-Plus clean")
    console.detail("Runs", f"{run_count:,}")
    console.detail("Preserved", "downloaded data, workspaces, index, and runtime")
    if not confirmed:
        if not sys.stdin.isatty():
            console.error("confirmation required; rerun with --yes")
            return 1
        answer = console.prompt(
            "Delete all runs and generated reports? [y/N] "
        ).strip().lower()
        if answer not in {"y", "yes"}:
            console.warning("Nothing removed")
            return 0

    try:
        if runs.exists():
            shutil.rmtree(runs)
    except OSError as error:
        console.error(f"could not remove benchmark results: {error}")
        return 1
    console.success(f"Removed {run_count:,} runs and generated reports")
    return 0


def _quality_change(baseline: float, treatment: float) -> tuple[str, bool | None]:
    difference = treatment - baseline
    if abs(difference) < 0.005:
        return "Δ 0.00 pp", None
    return f"Δ {difference:+.2f} pp", difference > 0


def _resource_change(
    baseline: float, treatment: float, *, speedup: bool = False
) -> tuple[str, bool | None]:
    if baseline == treatment:
        return "no change", None
    if baseline == 0:
        return "increased from zero", False
    difference = 100 * abs(treatment - baseline) / baseline
    direction = "less" if treatment < baseline else "more"
    rendered = f"{difference:.1f}% {direction}"
    if speedup and treatment > 0 and treatment < baseline:
        rendered += f" · {baseline / treatment:.2f}× speedup"
    elif speedup and treatment > baseline:
        rendered += f" · {treatment / baseline:.2f}× slower"
    return rendered, treatment < baseline


def _show_run_result(console: Console, root: Path) -> int:
    metadata = read_json(root / "run.json")
    completed = int(metadata.get("completed_cases", 0))
    planned = len(metadata["query_ids"])
    console.blank()
    if completed == planned:
        console.success("Run complete")
    else:
        console.warning(
            f"Run finished with incomplete cases: {completed} / {planned} completed"
        )
    console.identifier("Run", root.name)
    console.detail("Artifacts", root)
    return 0 if completed == planned else 1


def _show_report(console: Console, report: Path) -> None:
    summary = json.loads((report / "summary.json").read_text(encoding="utf-8"))
    answer = summary["quality"]
    baseline = summary["profiles"]["baseline"]
    treatment = summary["profiles"]["zvec-grep"]
    console.success("Report generated")
    console.detail(
        "Run",
        f"{summary['run_id']} · {summary['suite']} · {summary['model']} · "
        f"reasoning {summary['reasoning_effort']}",
    )
    console.detail(
        "Cases",
        f"{summary['cases']['completed']} / {summary['cases']['planned']} completed",
    )
    console.detail(
        "Trials",
        f"{summary['trials']['completed']} / {summary['trials']['planned']} completed",
    )
    if answer.get("status") in {"scored", "partial"}:
        baseline_quality = float(answer["baseline_accuracy_percent"])
        treatment_quality = float(answer["treatment_accuracy_percent"])
        change, favorable = _quality_change(baseline_quality, treatment_quality)
        console.metric(
            "Quality",
            f"baseline {baseline_quality:.2f}% · "
            f"zvec-grep {treatment_quality:.2f}% · "
            f"{answer['scored_trials']} / {summary['trials']['completed']} scored"
            + (" (partial)" if answer["status"] == "partial" else ""),
            change,
            favorable=favorable,
        )
    else:
        console.detail("Quality", "pending evaluation")
    usage_quality = summary["zvec_grep_usage_audit"]
    if usage_quality.get("status") in {"scored", "partial"}:
        console.detail(
            "zvec-grep usage",
            f"{usage_quality['correct_trials']} / "
            f"{usage_quality['evaluated_trials']} correct",
        )
    elif usage_quality.get("status") == "pending":
        console.detail("zvec-grep usage", "pending smoke-test audit")
    baseline_token_summary = baseline["tokens"]["input"]
    treatment_token_summary = treatment["tokens"]["input"]
    if (
        baseline_token_summary["available"]
        and treatment_token_summary["available"]
    ):
        baseline_tokens = float(baseline_token_summary["total"])
        treatment_tokens = float(treatment_token_summary["total"])
        change, favorable = _resource_change(baseline_tokens, treatment_tokens)
        console.metric(
            "Input tokens",
            f"baseline {baseline_tokens:,.0f} · "
            f"zvec-grep {treatment_tokens:,.0f}",
            change,
            favorable=favorable,
        )
    else:
        console.detail("Input tokens", "unavailable")
    change, favorable = _resource_change(
        float(baseline["wall_seconds"]["total"]),
        float(treatment["wall_seconds"]["total"]),
        speedup=True,
    )
    console.metric(
        "Agent time",
        f"baseline {baseline['wall_seconds']['total']:,.1f}s · "
        f"zvec-grep {treatment['wall_seconds']['total']:,.1f}s",
        change,
        favorable=favorable,
    )
    console.detail("Report", report / "summary.md")


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    config = load_config()

    if args.command == "doctor":
        report = run_doctor(
            config,
            codex_bin=args.codex_bin,
        )
        color = (
            sys.stdout.isatty()
            and "NO_COLOR" not in os.environ
            and os.environ.get("TERM") != "dumb"
        )
        print(
            json.dumps(report, indent=2)
            if args.json
            else format_report(report, color=color)
        )
        return 0 if report["ready"] else 1
    artifacts = DEFAULT_ARTIFACTS_DIR.resolve()
    if args.command == "prepare":
        console = Console()
        console.heading("BrowseComp-Plus prepare")
        console.detail("Artifacts", artifacts)

        console.step(1, 3, "Prepare official data")
        try:
            dataset_state = prepared_dataset(config, artifacts)
        except RuntimeError as error:
            console.error(str(error))
            return 1
        if dataset_state:
            console.success("Reused prepared official data")
            console.detail("State", dataset_state)
        else:
            console.activity("Downloading pinned Hugging Face snapshots")
            try:
                fetch(
                    config,
                    artifacts,
                    token=args.hf_token or os.environ.get("HF_TOKEN"),
                    status=console.activity,
                    progress=console.progress,
                )
            except RuntimeError as error:
                console.error(str(error))
                return 1
            console.success("Official data downloaded and verified")

        console.step(2, 3, "Prepare workspaces")
        try:
            corpus_state = prepared_corpus(config, artifacts)
        except RuntimeError as error:
            console.error(str(error))
            return 1
        if corpus_state:
            console.success(
                "Reused two workspaces with "
                f"{config.dataset.expected_corpus_documents:,} documents each"
            )
            console.detail("State", corpus_state)
        else:
            console.activity("Writing two independent corpus copies")
            try:
                materialize(
                    config,
                    artifacts,
                    progress=console.progress,
                )
            except RuntimeError as error:
                console.error(str(error))
                return 1
            finally:
                console.finish_progress()
            console.success(
                "Prepared two workspaces with "
                f"{config.dataset.expected_corpus_documents:,} documents each"
            )

        index_dir = workspace_root(artifacts, "zvec-grep") / ".zvec-grep"
        existing_index = prepared_index(config, artifacts)
        reuse_index = existing_index is not None and index_is_ready(
            config,
            artifacts,
        )
        rebuild_index = index_dir.is_dir() and not reuse_index
        if not reuse_index and not args.yes:
            if not sys.stdin.isatty():
                action = "rebuild" if rebuild_index else "build"
                raise SystemExit(f"error: index {action} requires --yes")
            prompt = (
                "Existing index cannot be reused and will be rebuilt. "
                "Continue? [y/N] "
                if rebuild_index
                else "Build the reusable zvec-grep index now? [y/N] "
            )
            answer = console.prompt(
                prompt
            ).strip().lower()
            if answer not in {"y", "yes"}:
                raise SystemExit("index preparation not confirmed")

        console.step(3, 3, "Prepare zvec-grep index")
        console.detail("Embedding", config.zvec_grep.embedding)
        console.detail("Concurrency", str(config.zvec_grep.embedding_concurrency))
        console.detail("Logs", artifacts / "logs")
        activity = (
            "Checking existing index"
            if reuse_index
            else "Rebuilding existing index"
            if rebuild_index
            else "Building reusable index"
        )
        console.activity(activity)
        if reuse_index:
            output = existing_index
        else:
            try:
                output = build_index(
                    config,
                    artifacts,
                    rebuild=rebuild_index,
                )
            except RuntimeError as error:
                console.error(str(error))
                return 1
        result = (
            "Reused existing index"
            if reuse_index
            else "Index rebuilt"
            if rebuild_index
            else "Index is ready"
        )
        console.success(result)
        console.detail("State", output)
        index_state = read_json(output)
        console.detail(
            "Index build wall time",
            f"{float(index_state['build_wall_seconds']):,.1f}s",
        )
        console.blank()
        console.success("Preparation complete")
        return 0
    if args.command == "run":
        console = Console()
        console.heading("BrowseComp-Plus run")
        try:
            root = run_benchmark(
                config,
                artifacts,
                suite=args.suite,
                codex_bin=args.codex_bin,
            )
        except (OSError, RuntimeError, ValueError) as error:
            console.error(str(error))
            return 1
        except RunTerminated as error:
            console.warning(
                "Run terminated; use 'zg-bench resume <run-id>' to continue"
            )
            return 128 + error.signum
        except KeyboardInterrupt:
            console.warning(
                "Run interrupted; use 'zg-bench resume <run-id>' to continue"
            )
            return 130
        return _show_run_result(console, root)
    if args.command == "resume":
        console = Console()
        console.heading("BrowseComp-Plus resume")
        try:
            root = resume_benchmark(
                config,
                artifacts,
                args.run_id,
                codex_bin=args.codex_bin,
            )
        except (OSError, RuntimeError, ValueError) as error:
            console.error(str(error))
            return 1
        except RunTerminated as error:
            console.warning("Run terminated; use the same resume command to continue")
            return 128 + error.signum
        except KeyboardInterrupt:
            console.warning("Run interrupted; use the same resume command to continue")
            return 130
        return _show_run_result(console, root)
    if args.command == "status":
        try:
            return _status(artifacts, args.run_id)
        except (OSError, RuntimeError, ValueError) as error:
            Console().error(str(error))
            return 1
    if args.command == "inspect":
        try:
            run_root = find_run(artifacts, args.run_id)
        except (OSError, RuntimeError, ValueError) as error:
            Console().error(str(error))
            return 1
        metadata = read_json(run_root / "run.json")
        results = []
        planned_trials = int(metadata["trials_per_case"])
        for trial_index in range(1, planned_trials + 1):
            result = (
                run_root
                / "cases"
                / args.case
                / args.profile
                / "trials"
                / f"trial-{trial_index:03d}"
                / "result.json"
            )
            if result.is_file():
                results.append(read_json(result))
        if not results:
            raise SystemExit(
                "error: no persisted trials found for "
                f"case {args.case}, profile {args.profile}"
            )
        print(
            json.dumps(
                {
                    "case": args.case,
                    "profile": args.profile,
                    "planned_trials": planned_trials,
                    "persisted_trials": len(results),
                    "trials": results,
                },
                indent=2,
                ensure_ascii=False,
            )
        )
        if args.events:
            for raw in results:
                print(f"\n--- trial {raw['trial_index']} events ---")
                print(
                    Path(raw["paths"]["events"]).read_text(encoding="utf-8"),
                    end="",
                )
        return 0
    if args.command == "evaluate":
        console = Console()
        console.heading("BrowseComp-Plus evaluate")
        try:
            run_root = _selected_run(artifacts, args.run_id)
            console.identifier("Run", run_root.name)
            already_complete = evaluation_complete(run_root)
            report = evaluate(
                artifacts,
                run_root.name,
                codex_bin=args.codex_bin,
                console=console,
            )
        except (OSError, RuntimeError, ValueError) as error:
            console.error(str(error))
            return 1
        console.blank()
        if already_complete:
            console.success("Evaluation already complete")
        _show_report(console, report)
        complete = (
            json.loads(
                (report.parent / "evaluation" / "summary.json").read_text(
                    encoding="utf-8"
                )
            )["status"]
            == "completed"
        )
        return 0 if complete else 1
    if args.command == "report":
        console = Console()
        console.heading("BrowseComp-Plus report")
        try:
            run_root = _selected_run(artifacts, args.run_id)
            console.identifier("Run", run_root.name)
            report = generate_report(run_root)
            _show_report(console, report)
        except (OSError, RuntimeError, ValueError) as error:
            console.error(str(error))
            return 1
        return 0
    if args.command == "clean":
        return _clean(artifacts, confirmed=args.yes)
    raise AssertionError(f"unhandled command: {args.command}")


if __name__ == "__main__":
    raise SystemExit(main())
