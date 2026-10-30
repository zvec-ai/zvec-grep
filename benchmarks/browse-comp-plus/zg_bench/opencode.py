from __future__ import annotations

import json
import os
import re
import signal
import subprocess
import sys
import threading
import time
from datetime import UTC, datetime
from pathlib import Path
from typing import BinaryIO

from .artifacts import atomic_write_text, write_json
from .config import BenchmarkConfig
from .corpus import workspace_root
from .models import AttemptResult, Profile, TraceSummary
from .process import resolve_executable, run_command
from .profiles import ensure_server, opencode_environment, server_environment
from .trace import parse_trace

INFRASTRUCTURE_PATTERN = (
    "stream disconnected",
    "failed to connect",
    "connection reset",
    "dns",
    "timed out",
    "timeout",
    "rate limit",
    "service unavailable",
)
HTTP_RETRY_PATTERN = re.compile(
    r"\b(?:http(?:/\d(?:\.\d)?)?|status(?:[ _-]?code)?)\s*"
    r"(?:[=:]\s*|error\s+)?(?:429|5\d{2})\b"
    r"|\b429\s+too many requests\b"
    r"|\b5\d{2}\s+(?:internal server error|bad gateway|service unavailable|"
    r"gateway timeout|server error|overloaded)\b",
    re.IGNORECASE,
)

HEARTBEAT_SECONDS = 30


def _iso_now() -> str:
    return datetime.now(UTC).isoformat()


def _elapsed(seconds: float) -> str:
    minutes, seconds = divmod(int(seconds), 60)
    hours, minutes = divmod(minutes, 60)
    return (
        f"{hours:02d}:{minutes:02d}:{seconds:02d}"
        if hours
        else f"{minutes:02d}:{seconds:02d}"
    )


def _progress(message: str, width: int) -> int:
    if not sys.stderr.isatty():
        print(message, file=sys.stderr, flush=True)
        return width
    width = max(width, len(message))
    print(f"\r{message:<{width}}", end="", file=sys.stderr, flush=True)
    return width


def _clear_progress(width: int) -> None:
    if sys.stderr.isatty() and width:
        print(f"\r{'':<{width}}\r", end="", file=sys.stderr, flush=True)


def _copy_stream(
    source: BinaryIO, destination: BinaryIO, activity: list[float]
) -> None:
    # BufferedReader.read(n) may wait for all n bytes, hiding short JSONL events
    # and making an active process appear idle. Pipes expose read1(), which
    # returns currently available bytes instead.
    read_available = getattr(source, "read1", source.read)
    while chunk := read_available(64 * 1024):
        destination.write(chunk)
        destination.flush()
        activity[0] = time.monotonic()


def _terminate_group(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def _corpus_access_denied(trace: TraceSummary, stderr: str) -> bool:
    denial = (
        "operation_not_permitted" in stderr.lower()
        or "sandbox violation" in stderr.lower()
    )
    message = trace.last_agent_message.lower()
    inaccessible = any(
        marker in message
        for marker in (
            "cannot access the corpus",
            "can't access the corpus",
            "shell cannot access the corpus",
            "local shell is unavailable under this corpus sandbox",
        )
    )
    return denial and inaccessible


def _usage_limit_exhausted(errors: str) -> bool:
    # Providers may report an exhausted account balance as a retryable HTTP 429.
    # Such failures require account action, unlike temporary request throttling.
    compact = re.sub(r"[^a-z0-9]", "", errors.lower())
    return any(
        marker in compact
        for marker in (
            "apiusagelimiterror",
            "insufficientquota",
            "quotaexhausted",
            "quotaexceeded",
            "exceededyourcurrentquota",
            "insufficientbalance",
            "billinghardlimit",
            "usagelimitreached",
            "usagelimitexceeded",
            "freeallocatedquotaexceeded",
        )
    )


def _retryable_error(error: str) -> bool:
    # TraceSummary preserves OpenCode's error object as JSON, including the
    # provider's APIError.data.isRetryable/statusCode and nested causes.
    try:
        pending = [json.loads(error)]
    except ValueError:
        pending = []
    retry_flags: list[bool] = []
    statuses: list[int] = []
    while pending:
        value = pending.pop()
        if isinstance(value, dict):
            if type(value.get("isRetryable")) is bool:
                retry_flags.append(value["isRetryable"])
            for key in ("statusCode", "status_code", "status"):
                status = value.get(key)
                if type(status) is int:
                    statuses.append(status)
            pending.extend(value.values())
        elif isinstance(value, list):
            pending.extend(value)
    if retry_flags:
        return all(retry_flags)
    if any(status == 429 or 500 <= status <= 599 for status in statuses):
        return True
    lowered = error.lower()
    return bool(HTTP_RETRY_PATTERN.search(error)) or any(
        marker in lowered for marker in INFRASTRUCTURE_PATTERN
    )


def _classify_attempt(
    *,
    exit_code: int,
    idle_killed: bool,
    trace: TraceSummary,
    stderr: str,
) -> tuple[str, bool]:
    completed = (
        exit_code == 0
        and not idle_killed
        and trace.turn_completed
        and not trace.errors
        and bool(trace.final_response.strip())
    )
    corpus_access_denied = _corpus_access_denied(trace, stderr)
    if completed and not corpus_access_denied:
        return "completed", False
    errors = (*trace.errors, stderr)
    if _usage_limit_exhausted("\n".join(errors)):
        return "failed", False
    infrastructure_failure = corpus_access_denied or (
        not completed
        and (
            idle_killed
            or exit_code < 0
            or any(_retryable_error(error) for error in errors)
        )
    )
    if infrastructure_failure:
        return "infrastructure_failed", True
    return "failed", False


def validate_model(_artifacts: Path, model: str) -> None:
    if model != "custom-openai/qwen3.8-max":
        raise ValueError("BrowseComp-Plus requires custom-openai/qwen3.8-max")


def run_prompt(
    executable: Path,
    *,
    home: Path,
    workspace: Path,
    model: str,
    prompt: str,
    output_dir: Path,
    idle_timeout_seconds: int | None = 1800,
    timeout_seconds: int | None = None,
    environment: dict[str, str] | None = None,
) -> dict:
    """Execute one isolated session, preserving unmodified OpenCode JSONL.

    The command has a fixed title to avoid an extra title-model request. The
    profile's explicit permission denials remain effective with --auto.
    """
    output_dir = output_dir.resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    events_path = output_dir / "events.jsonl"
    stderr_path = output_dir / "stderr.log"
    final_path = output_dir / "response.md"
    atomic_write_text(output_dir / "prompt.md", prompt)
    command = [
        str(executable),
        "run",
        "--pure",
        "--auto",
        "--format",
        "json",
        "--thinking",
        "--title",
        "BrowseComp-Plus benchmark",
        "--model",
        model,
    ]
    process_environment = opencode_environment(home)
    process_environment["GIT_CEILING_DIRECTORIES"] = str(workspace.parent)
    if environment:
        process_environment.update(environment)
    write_json(
        output_dir / "command.json",
        {
            "args": command,
            "cwd": str(workspace),
            "model": model,
            "trace_format": "opencode-jsonl-v1.18.4",
            "usage_scope": "main-session; task delegation disabled in profile",
            "environment": {
                key: process_environment[key]
                for key in (
                    "HOME",
                    "OPENCODE_CONFIG",
                    "GIT_CEILING_DIRECTORIES",
                    "ZVEC_GREP_HOME",
                    "ZVEC_GREP_SERVER_URL",
                )
                if key in process_environment
            },
        },
    )
    started = time.monotonic()
    activity = [started]
    idle_killed = False
    progress_width = 0
    next_heartbeat = started + HEARTBEAT_SECONDS
    with events_path.open("wb") as events, stderr_path.open("wb") as errors:
        process = subprocess.Popen(
            command,
            cwd=workspace,
            env=process_environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        threads = []
        try:
            assert process.stdin and process.stdout and process.stderr
            for source, destination in (
                (process.stdout, events),
                (process.stderr, errors),
            ):
                thread = threading.Thread(
                    target=_copy_stream,
                    args=(source, destination, activity),
                    daemon=True,
                )
                thread.start()
                threads.append(thread)

            def send_prompt() -> None:
                try:
                    process.stdin.write(prompt.encode("utf-8"))
                    process.stdin.close()
                except BrokenPipeError:
                    # Invalid configuration can terminate before reading stdin.
                    pass

            # Large judge prompts must not block the timeout monitor while a
            # stalled process has yet to consume stdin.
            writer = threading.Thread(target=send_prompt, daemon=True)
            writer.start()
            threads.append(writer)
            while process.poll() is None:
                now = time.monotonic()
                if now >= next_heartbeat:
                    progress_width = _progress(
                        f"· OpenCode · elapsed {_elapsed(now - started)} · "
                        f"last activity {_elapsed(now - activity[0])} ago",
                        progress_width,
                    )
                    next_heartbeat = now + HEARTBEAT_SECONDS
                if (
                    idle_timeout_seconds and now - activity[0] > idle_timeout_seconds
                ) or (timeout_seconds and now - started > timeout_seconds):
                    idle_killed = True
                    _terminate_group(process)
                    break
                time.sleep(0.1)
            exit_code = process.wait()
        except BaseException:
            _terminate_group(process)
            raise
        finally:
            for thread in threads:
                thread.join(timeout=5)
            for pipe in (process.stdin, process.stdout, process.stderr):
                if pipe is not None:
                    pipe.close()
            _clear_progress(progress_width)
    trace = parse_trace(events_path)
    atomic_write_text(final_path, trace.final_response)
    return {
        "exit_code": exit_code,
        "idle_killed": idle_killed,
        "trace": trace,
        "wall_seconds": time.monotonic() - started,
    }


def run_attempt(
    config: BenchmarkConfig,
    artifacts: Path,
    *,
    query_id: str,
    trial_index: int,
    prompt: str,
    profile: Profile,
    model: str,
    reasoning_effort: str,
    attempt: int,
    output_dir: Path,
    profiles_root: Path,
    opencode_bin: str = "opencode",
    idle_timeout_seconds: int | None = None,
    shared_runtime: bool = False,
) -> AttemptResult:
    opencode = resolve_executable(opencode_bin)
    if opencode is None:
        raise RuntimeError(f"OpenCode executable not found: {opencode_bin}")
    home = (profiles_root / profile / "opencode-home").resolve()
    if not home.is_dir():
        raise RuntimeError("run-local OpenCode profiles are missing")
    environment = {}
    if profile == "zvec-grep":
        if shared_runtime:
            check = run_command(
                ["zg", "--server", "status", "--check-ready"],
                env=server_environment(config, artifacts),
                timeout=30,
            )
            if not check.ok:
                raise RuntimeError("shared zvec-grep server is unavailable")
        else:
            ensure_server(config, artifacts)
        index_dir = workspace_root(artifacts, "zvec-grep") / ".zvec-grep"
        if not index_dir.is_dir():
            raise RuntimeError("zvec-grep index is missing; run 'zg-bench prepare'")
        server_env = server_environment(config, artifacts)
        environment = {
            key: server_env[key] for key in ("ZVEC_GREP_HOME", "ZVEC_GREP_SERVER_URL")
        }
    server_log = artifacts / "runtime" / "zvec-home" / "daemon" / "logs" / "server.log"
    server_log_offset = (
        server_log.stat().st_size
        if profile == "zvec-grep" and server_log.is_file()
        else 0
    )
    _progress(
        f"→ case {query_id} · trial {trial_index} · {profile} · attempt {attempt}", 0
    )
    started_at = _iso_now()
    execution = run_prompt(
        opencode,
        home=home,
        workspace=workspace_root(artifacts, profile),
        model=model,
        prompt=prompt,
        output_dir=output_dir,
        idle_timeout_seconds=idle_timeout_seconds,
        environment=environment,
    )
    finished_at = _iso_now()
    trace = execution["trace"]
    stderr_path = output_dir / "stderr.log"
    status, infrastructure_failure = _classify_attempt(
        exit_code=execution["exit_code"],
        idle_killed=execution["idle_killed"],
        trace=trace,
        stderr=stderr_path.read_text(encoding="utf-8", errors="replace"),
    )
    paths = {
        "events": str((output_dir / "events.jsonl").resolve()),
        "stderr": str(stderr_path.resolve()),
        "response": str((output_dir / "response.md").resolve()),
    }
    if profile == "zvec-grep" and server_log.is_file():
        with server_log.open("rb") as source:
            source.seek(min(server_log_offset, server_log.stat().st_size))
            server_trace = source.read().decode("utf-8", errors="replace")
        server_trace_path = output_dir / "zvec-server.jsonl"
        atomic_write_text(server_trace_path, server_trace)
        paths["zvec_server"] = str(server_trace_path.resolve())
    result = AttemptResult(
        query_id=query_id,
        profile=profile,
        trial_index=trial_index,
        status=status,
        attempt=attempt,
        started_at=started_at,
        finished_at=finished_at,
        wall_seconds=execution["wall_seconds"],
        exit_code=execution["exit_code"],
        infrastructure_failure=infrastructure_failure,
        trace=trace,
        paths=paths,
    )
    write_json(output_dir / "result.json", result.to_dict())
    write_json(
        output_dir / "usage.json", trace.usage.to_dict() if trace.usage else None
    )
    write_json(output_dir / "tools.json", [call.to_dict() for call in trace.tool_calls])
    write_json(output_dir / "documents.json", list(trace.observed_docids))
    return result
