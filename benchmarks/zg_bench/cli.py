from __future__ import annotations

import argparse
import shlex
from pathlib import Path

from .doctor import run_doctor
from .runner import (
    DEFAULT_RUNS_DIR,
    PROFILE_SELECTIONS,
    SuiteConfigError,
    available_suites,
    build_harbor_command,
    execute,
    execution_environment,
    load_suite,
    new_run_id,
    prepare_setup_cache,
    profile_job_name,
    selected_profiles,
    uses_setup_cache,
    validate_profile_credentials,
    zvec_grep_package_install_spec,
)
from .settings import ZVEC_GREP_PACKAGE


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="zg-bench",
        description="Run the zvec-grep benchmark suites through Harbor.",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    subparsers.add_parser(
        "doctor", help="check that the local benchmark dependencies are ready"
    )

    run = subparsers.add_parser("run", help="run a smoke benchmark")
    run.add_argument("suite", choices=available_suites())
    run.add_argument("--agent", required=True, help="Harbor agent name")
    run.add_argument("--model", required=True, help="model identifier for the agent")
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
        return run_doctor()

    try:
        suite = load_suite(args.suite)
        profiles = selected_profiles(args.profile)
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
            validate_profile_credentials(
                profiles,
                agent=args.agent,
                model=args.model,
            )
    except (SuiteConfigError, ValueError) as error:
        raise SystemExit(f"error: {error}") from error

    print(f"Suite:   {suite.name}")
    print("Tier:    smoke")
    print(f"Profile: {args.profile}")
    print(f"Task:    {suite.task}")
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
    except (FileNotFoundError, RuntimeError) as error:
        raise SystemExit(
            "error: benchmark setup failed; run 'zg-bench doctor' to check the "
            f"setup ({error})"
        ) from error
    return return_code


if __name__ == "__main__":
    raise SystemExit(main())
