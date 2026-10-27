"""Credential setup and allowlisted evidence export for manual GitHub Actions runs."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import stat
import subprocess
import tempfile
from pathlib import Path

from .config import DEFAULT_ARTIFACTS_DIR, DEFAULT_CONFIG_PATH, load_config
from .process import resolve_executable
from .profiles import opencode_environment, prepare_judge_home

SECRET_ENV = ("OPENAI_API_KEY", "ZVEC_GREP_API_KEY", "HF_TOKEN")


def prepare_sources() -> None:
    """Persist validated public data before starting the expensive index build."""
    from .dataset import fetch, prepared_dataset

    config = load_config()
    if prepared_dataset(config, DEFAULT_ARTIFACTS_DIR) is None:
        fetch(config, DEFAULT_ARTIFACTS_DIR, token=os.environ.get("HF_TOKEN"))
    print("Pinned source snapshots are ready for caching.")


def configure_auth(home: Path) -> None:
    config = load_config()
    if not config.zvec_grep.embedding.startswith("local/"):
        if not os.environ.get("ZVEC_GREP_API_KEY"):
            raise ValueError("QWEN_EMBEDDING_API_KEY Actions secret is required")
        if not os.environ.get("ZVEC_GREP_ENDPOINT"):
            raise ValueError("QWEN_EMBEDDING_ENDPOINT Actions variable is required")
    if not os.environ.get("OPENAI_API_KEY", "").strip():
        raise ValueError("QWEN_API_KEY (or GLM_API_KEY) Actions secret is required")
    if not os.environ.get("BROWSE_COMP_MODEL_BASE_URL", "").strip():
        raise ValueError(
            "BROWSE_COMP_MODEL_BASE_URL (or SWE_QA_MODEL_BASE_URL) Actions variable "
            "must specify the model's OpenAI-compatible base URL"
        )
    # Fail before the expensive full-corpus preparation if the pinned CLI is incompatible.
    help_result = subprocess.run(
        ["opencode", "run", "--help"],
        check=True,
        capture_output=True,
        text=True,
        env=opencode_environment(home),
    )
    help_text = help_result.stdout + help_result.stderr
    for flag in ("--format", "--model", "--agent", "--dir", "--pure"):
        if flag not in help_text:
            raise ValueError(
                f"installed OpenCode does not support required flag {flag}"
            )
    home.mkdir(mode=0o700, parents=True, exist_ok=True)
    home.chmod(0o700)
    prepare_judge_home(config, DEFAULT_ARTIFACTS_DIR, home)


def verify_model() -> None:
    """Make one small model call before paying for a full-corpus index build."""
    from .opencode import run_prompt

    config = load_config()
    executable = resolve_executable("opencode")
    if executable is None:
        raise ValueError("OpenCode executable is required")
    with tempfile.TemporaryDirectory(prefix="browse-comp-plus-preflight-") as directory:
        root = Path(directory)
        workspace = root / "workspace"
        workspace.mkdir()
        home = prepare_judge_home(config, root / "artifacts", root / "home")
        result = run_prompt(
            executable,
            home=home,
            workspace=workspace,
            model=config.run.model,
            prompt="Reply exactly OK. Do not use tools.",
            output_dir=root / "response",
            idle_timeout_seconds=180,
            timeout_seconds=180,
        )
        if (
            result["exit_code"] != 0
            or result["idle_killed"]
            or not result["trace"].turn_completed
            or not result["trace"].final_response.strip()
            or result["trace"].errors
        ):
            raise ValueError(
                f"OpenCode preflight failed; verify credential access to {config.run.model} "
                "and service availability before preparing the corpus"
            )


def secret_values(home: Path) -> set[bytes]:
    # OpenCode configs reference environment variables; no persistent account login.
    del home
    values = {os.environ[name] for name in SECRET_ENV if os.environ.get(name)}
    values.update(json.dumps(value)[1:-1] for value in tuple(values))
    return {value.encode() for value in values if value}


def _contains_secret(path: Path, secrets: set[bytes]) -> bool:
    overlap = max((len(value) for value in secrets), default=1) - 1
    tail = b""
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            window = tail + chunk
            if any(value in window for value in secrets):
                return True
            tail = window[-overlap:] if overlap else b""
    return False


def _copy_evidence(source: Path, destination: Path, secrets: set[bytes]) -> None:
    if source.is_symlink():
        raise ValueError("refusing to export a symlink in benchmark evidence")
    if not source.exists():
        return
    if source.is_dir():
        for child in sorted(source.iterdir()):
            _copy_evidence(child, destination / child.name, secrets)
        return
    if not stat.S_ISREG(source.stat().st_mode):
        raise ValueError("refusing to export a non-regular benchmark file")
    if source.name == "auth.json" or _contains_secret(source, secrets):
        raise ValueError(
            "credential material detected; benchmark evidence will not be uploaded"
        )
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)


def export_evidence(
    artifacts: Path, output: Path, home: Path, manifest: Path, plan: Path | None = None
) -> None:
    if output.exists():
        raise ValueError("evidence output must be a new directory")
    secrets = secret_values(home)
    output.mkdir(parents=True)
    try:
        _copy_evidence(DEFAULT_CONFIG_PATH, output / "benchmark.toml", secrets)
        if not manifest.is_file():
            raise ValueError("candidate manifest is missing")
        _copy_evidence(manifest, output / "candidate-manifest.json", secrets)
        for name in ("dataset.json", "corpus.json", "index.json", "runtime.json"):
            _copy_evidence(artifacts / "state" / name, output / "state" / name, secrets)
        _copy_evidence(artifacts / "logs", output / "logs", secrets)
        runs = artifacts / "runs"
        if runs.is_symlink():
            raise ValueError("refusing to export a symlink in benchmark evidence")
        for run in sorted(runs.iterdir()) if runs.is_dir() else []:
            if run.is_symlink():
                raise ValueError("refusing to export a symlink in benchmark evidence")
            if not run.is_dir():
                continue
            # Never traverse profile homes, runtime, or the corpus.
            for name in (
                "run.json",
                "report",
                "cases",
                "evaluation",
                "shards",
                "resume",
            ):
                _copy_evidence(run / name, output / "runs" / run.name / name, secrets)
        identity = {
            "candidate_commit": os.environ.get("BROWSE_CANDIDATE_COMMIT", ""),
            "harness_commit": os.environ.get("BROWSE_HARNESS_COMMIT", ""),
            "workflow_run_id": os.environ.get("GITHUB_RUN_ID", ""),
            "workflow_run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT", ""),
            "case_id": os.environ.get("BROWSE_CASE", ""),
        }
        if plan:
            from .artifacts import sha256_file

            identity["plan_sha256"] = sha256_file(plan)
            _copy_evidence(plan, output / "plan.json", secrets)
        (output / "ci.json").write_text(
            json.dumps(identity, indent=2) + "\n", encoding="utf-8"
        )
        from .ci_resume import seal_evidence

        seal_evidence(output)
    except BaseException:
        shutil.rmtree(output)
        raise
    if summary_path := os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(summary_path).open("a", encoding="utf-8") as summary:
            summary.write("## BrowseComp-Plus CI\n\n")
            summary.write(f"Candidate: `{identity['candidate_commit']}`  \n")
            summary.write(f"Harness: `{identity['harness_commit']}`\n\n")
            reports = sorted(output.glob("runs/*/report/summary.md"))
            if not reports:
                summary.write(
                    "No paired report was produced. See failed steps and diagnostic artifacts.\n"
                )
            summary.writelines(
                report.read_text(encoding="utf-8") + "\n" for report in reports
            )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("auth")
    commands.add_parser("preflight")
    commands.add_parser("fetch")
    export = commands.add_parser("export")
    export.add_argument("--output", type=Path, required=True)
    export.add_argument("--candidate-manifest", type=Path, required=True)
    export.add_argument("--plan", type=Path)
    args = parser.parse_args()
    try:
        home = Path(os.environ["OPENCODE_HOME"])
        if args.command == "auth":
            configure_auth(home)
        elif args.command == "preflight":
            verify_model()
        elif args.command == "fetch":
            prepare_sources()
        else:
            export_evidence(
                DEFAULT_ARTIFACTS_DIR,
                args.output,
                home,
                args.candidate_manifest,
                args.plan,
            )
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"BrowseComp-Plus CI failed: {error}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
