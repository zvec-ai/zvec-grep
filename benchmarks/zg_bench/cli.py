from __future__ import annotations

import argparse
import shlex
import sys
from pathlib import Path

from .diagnostics import format_job_diagnostics, latest_job
from .doctor import collect_checks, print_report, run_doctor
from .runner import (
    DEFAULT_RUNS_DIR,
    PROFILE_SELECTIONS,
    TIERS,
    SuiteConfigError,
    available_agent_models,
    available_suites,
    build_harbor_command,
    execute,
    execution_environment,
    load_suite,
    new_run_id,
    prepare_setup_cache,
    profile_job_name,
    resolve_agent_model,
    selected_profiles,
    uses_setup_cache,
    validate_job_destinations,
    validate_zvec_grep_package_compatibility,
    zvec_grep_package_install_spec,
)
from .settings import ZVEC_GREP_EMBEDDING, ZVEC_GREP_PACKAGE


class BenchmarkArgumentParser(argparse.ArgumentParser):
    def parse_args(
        self,
        args: list[str] | None = None,
        namespace: argparse.Namespace | None = None,
    ) -> argparse.Namespace:
        parsed = super().parse_args(args, namespace)
        if parsed.command not in {"doctor", "run"}:
            return parsed

        agent = getattr(parsed, "agent", None)
        model = getattr(parsed, "model", None)
        if (agent is None) != (model is None):
            self.error(f"{parsed.command} requires --agent and --model together")
        if agent is not None and model is not None:
            try:
                resolve_agent_model(agent, model)
            except ValueError as error:
                self.error(f"{error}; see 'zg-bench list agent-models'")
        return parsed


def build_parser() -> argparse.ArgumentParser:
    parser = BenchmarkArgumentParser(
        prog="zg-bench",
        description="Run the zvec-grep benchmark suites through Harbor.",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    doctor = subparsers.add_parser(
        "doctor", help="check that the local benchmark dependencies are ready"
    )
    doctor.add_argument("--agent", help="benchmark agent name for run-specific checks")
    doctor.add_argument("--model", help="model identifier for run-specific checks")
    doctor.add_argument(
        "--profile",
        choices=PROFILE_SELECTIONS,
        default="all",
        help="tool profile to check (default: all)",
    )
    doctor.add_argument(
        "--zvec-grep-package",
        default=ZVEC_GREP_PACKAGE,
        help="zvec-grep npm spec, version, local directory, or .tgz",
    )

    list_parser = subparsers.add_parser(
        "list", help="list suites, tier tasks, or supported agent/model pairs"
    )
    list_subparsers = list_parser.add_subparsers(dest="list_command", required=True)
    list_subparsers.add_parser("suites", help="list built-in benchmark suites")
    list_subparsers.add_parser(
        "agent-models", help="list supported agent/model combinations"
    )
    list_tasks = list_subparsers.add_parser("tasks", help="list tasks in a suite tier")
    list_tasks.add_argument("suite", help="built-in suite name or YAML path")
    list_tasks.add_argument("--tier", choices=TIERS, default="smoke")

    diagnose = subparsers.add_parser(
        "diagnose", help="show actionable failure details for a Harbor job"
    )
    diagnose.add_argument("job", nargs="?", help="job name or directory")
    diagnose.add_argument(
        "--latest", action="store_true", help="diagnose the most recently modified job"
    )
    diagnose.add_argument(
        "--jobs-dir",
        type=Path,
        default=DEFAULT_RUNS_DIR,
        help="directory containing Harbor jobs",
    )

    run = subparsers.add_parser("run", help="run a benchmark suite tier")
    run.add_argument(
        "suite",
        help="built-in suite name or YAML path; use 'zg-bench list suites'",
    )
    run.add_argument("--agent", required=True, help="benchmark agent name")
    run.add_argument("--model", required=True, help="model identifier for the agent")
    run.add_argument(
        "--tier",
        choices=TIERS,
        default="smoke",
        help="suite tier to run (default: smoke)",
    )
    run.add_argument(
        "--task",
        dest="tasks",
        action="append",
        metavar="TASK",
        help="override the tier task selection; may be repeated",
    )
    run.add_argument(
        "--profile",
        choices=PROFILE_SELECTIONS,
        default="all",
        help="tool profile to run (default: all)",
    )
    run.add_argument(
        "--jobs-dir",
        type=Path,
        default=DEFAULT_RUNS_DIR,
        help="directory for Harbor job output (default: benchmarks/runs)",
    )
    run.add_argument(
        "--job-name",
        help="override the job name; profile names are appended when running all",
    )
    run.add_argument(
        "--zvec-grep-package",
        default=ZVEC_GREP_PACKAGE,
        help=(
            "zvec-grep npm spec, version, local directory, or .tgz "
            f"(default: {ZVEC_GREP_PACKAGE})"
        ),
    )
    run.add_argument(
        "--dry-run",
        action="store_true",
        help="print the Harbor command without running it",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if args.command == "doctor":
        profiles = selected_profiles(args.profile) if args.agent is not None else ()
        return run_doctor(
            agent=args.agent,
            model=args.model,
            profiles=profiles,
            zvec_grep_package=args.zvec_grep_package,
        )

    if args.command == "list":
        if args.list_command == "suites":
            for suite_name in available_suites():
                print(suite_name)
            return 0
        if args.list_command == "agent-models":
            print(f"{'Agent':<12} {'Model':<24} Configuration")
            for support in available_agent_models():
                print(
                    f"{support.agent:<12} {support.model:<24} "
                    f"{support.configuration}"
                )
                if support.aliases:
                    print(f"{'':<12} aliases: {', '.join(support.aliases)}")
            return 0
        try:
            suite = load_suite(args.suite, tier=args.tier)
        except SuiteConfigError as error:
            raise SystemExit(f"error: {error}") from error
        print(f"Suite: {suite.name}")
        print(f"Tier:  {suite.tier}")
        if suite.tasks is None:
            print("Tasks: all dataset tasks")
        else:
            print(f"Tasks: {len(suite.tasks)}")
            for task in suite.tasks:
                print(f"  {task}")
        return 0

    if args.command == "diagnose":
        if args.latest and args.job:
            raise SystemExit("error: pass either a job or --latest, not both")
        if args.latest:
            job_dir = latest_job(args.jobs_dir)
            if job_dir is None:
                raise SystemExit(f"error: no jobs found in {args.jobs_dir}")
        elif args.job:
            candidate = Path(args.job).expanduser()
            job_dir = candidate if candidate.is_dir() else args.jobs_dir / args.job
        else:
            raise SystemExit("error: diagnose requires a job name/directory or --latest")
        print(format_job_diagnostics(job_dir))
        return 0

    try:
        suite = load_suite(args.suite, tier=args.tier, task_overrides=args.tasks)
        profiles = selected_profiles(args.profile)
        validate_zvec_grep_package_compatibility(
            profiles,
            agent=args.agent,
            zvec_grep_package=args.zvec_grep_package,
        )
        run_id = new_run_id()
        run_specs = [
            (
                profile,
                profile_job_name(
                    suite,
                    profile,
                    run_id=run_id,
                    override=args.job_name,
                    paired=len(profiles) > 1,
                ),
            )
            for profile in profiles
        ]
        dry_run_commands = [
            (
                profile,
                build_harbor_command(
                    suite,
                    profile=profile,
                    agent=args.agent,
                    model=args.model,
                    jobs_dir=args.jobs_dir,
                    job_name=job_name,
                    zvec_grep_package=zvec_grep_package_install_spec(
                        args.zvec_grep_package
                    ),
                ),
            )
            for profile, job_name in run_specs
        ]
        if not args.dry_run:
            print("Preflight:", flush=True)
            checks = collect_checks(
                agent=args.agent,
                model=args.model,
                profiles=profiles,
                zvec_grep_package=args.zvec_grep_package,
            )
            if print_report(checks) != 0:
                raise SystemExit(
                    "error: benchmark preflight failed; fix the checks above "
                    "before starting Docker"
                )
            validate_job_destinations(args.jobs_dir, run_specs)
    except (SuiteConfigError, ValueError) as error:
        raise SystemExit(f"error: {error}") from error

    print(f"Suite:   {suite.name}")
    print(f"Tier:    {suite.tier}")
    print(f"Profile: {args.profile}")
    if "zvec-grep" in profiles and ZVEC_GREP_EMBEDDING.startswith("qwen/"):
        print(
            f"Embedding: {ZVEC_GREP_EMBEDDING} "
            "(remote; authorized for each index command)"
        )
    if suite.tasks is None:
        print("Tasks:   all dataset tasks")
    else:
        print(f"Tasks:   {len(suite.tasks)}")
        for task in suite.tasks:
            print(f"  - {task}")
    if args.dry_run:
        for profile, command in dry_run_commands:
            print(f"{profile}: {shlex.join(command)}")
        return 0

    return_code = 0
    try:
        for profile, job_name in run_specs:
            print(f"Starting profile: {profile}", flush=True)
            prepared_cache = None
            if uses_setup_cache(args.agent):
                prepared_cache = prepare_setup_cache(
                    args.agent,
                    profile,
                    zvec_grep_package=args.zvec_grep_package,
                )
            command = build_harbor_command(
                suite,
                profile=profile,
                agent=args.agent,
                model=args.model,
                jobs_dir=args.jobs_dir,
                job_name=job_name,
                zvec_grep_package=(
                    prepared_cache.zvec_grep_package
                    if prepared_cache is not None
                    and prepared_cache.zvec_grep_package is not None
                    else zvec_grep_package_install_spec(args.zvec_grep_package)
                ),
                zvec_grep_package_sha256=(
                    prepared_cache.zvec_grep_package_sha256
                    if prepared_cache is not None
                    else None
                ),
            )
            profile_return_code = execute(
                command,
                jobs_dir=args.jobs_dir,
                environment=execution_environment(
                    agent=args.agent,
                    model=args.model,
                ),
            )
            if profile_return_code != 0 and return_code == 0:
                return_code = profile_return_code
            if profile_return_code != 0:
                print(
                    format_job_diagnostics(args.jobs_dir / job_name),
                    file=sys.stderr,
                    flush=True,
                )
    except (FileNotFoundError, RuntimeError) as error:
        raise SystemExit(
            "error: benchmark setup failed; run 'zg-bench doctor' to check the "
            f"setup ({error})"
        ) from error
    return return_code


if __name__ == "__main__":
    raise SystemExit(main())
