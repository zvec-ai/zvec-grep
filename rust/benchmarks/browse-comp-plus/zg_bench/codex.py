from __future__ import annotations

import os
import signal
import subprocess
import sys
import threading
import time
from datetime import UTC, datetime
from difflib import get_close_matches
from pathlib import Path
from typing import BinaryIO

from .artifacts import atomic_write_text, read_json, write_json
from .config import BenchmarkConfig
from .corpus import workspace_root
from .models import AttemptResult, Profile, TraceSummary
from .process import resolve_executable
from .profiles import ensure_server, server_environment
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


def _classify_attempt(
    *,
    exit_code: int,
    idle_killed: bool,
    trace: TraceSummary,
    stderr: str,
) -> tuple[str, bool]:
    completed = exit_code == 0 and trace.turn_completed and bool(
        trace.final_response.strip()
    )
    corpus_access_denied = _corpus_access_denied(trace, stderr)
    combined_errors = "\n".join((*trace.errors, stderr)).lower()
    infrastructure_failure = (
        corpus_access_denied
        or (
            not completed
            and (
                idle_killed
                or exit_code < 0
                or any(
                    marker in combined_errors for marker in INFRASTRUCTURE_PATTERN
                )
            )
        )
    )
    if completed and not corpus_access_denied:
        return "completed", False
    if infrastructure_failure:
        return "infrastructure_failed", True
    return "failed", False


def validate_model(artifacts: Path, model: str) -> None:
    """Reject model IDs absent from the authenticated Codex model cache."""
    source_home = Path(os.environ.get("CODEX_HOME", Path.home() / ".codex"))
    cache = source_home / "models_cache.json"
    if not cache.is_file():
        return
    raw_models = read_json(cache).get("models", [])
    models = {
        str(item["slug"]): item
        for item in raw_models
        if isinstance(item, dict) and item.get("slug")
    }
    if not models or model in models:
        return
    visible = sorted(
        slug for slug, item in models.items() if item.get("visibility") == "list"
    )
    suggestions = get_close_matches(model, visible, n=3, cutoff=0.45)
    if model.startswith("gpt-5.6"):
        suggestions = [slug for slug in visible if slug.startswith("gpt-5.6")]
    rendered = ", ".join(suggestions or visible)
    raise RuntimeError(
        f"Codex model {model!r} is not available for the authenticated account. "
        f"Available model IDs include: {rendered}"
    )


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
    codex_bin: str = "codex",
    idle_timeout_seconds: int | None = None,
) -> AttemptResult:
    codex = resolve_executable(codex_bin)
    if codex is None:
        raise RuntimeError(f"Codex executable not found: {codex_bin}")
    profile_root = (profiles_root / profile).resolve()
    codex_home = profile_root / "codex-home"
    if not codex_home.is_dir():
        raise RuntimeError("run-local Codex profiles are missing")
    if profile == "zvec-grep":
        ensure_server(config, artifacts)

    workspace = workspace_root(artifacts, profile)
    index_dir = workspace_root(artifacts, "zvec-grep") / ".zvec-grep"
    if profile == "zvec-grep" and not index_dir.is_dir():
        raise RuntimeError("zvec-grep index is missing; run 'zg-bench prepare'")

    output_dir = output_dir.resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    events_path = output_dir / "events.jsonl"
    stderr_path = output_dir / "stderr.log"
    final_path = output_dir / "response.md"
    atomic_write_text(output_dir / "prompt.md", prompt)
    server_log = artifacts / "runtime" / "zvec-home" / "daemon" / "logs" / "server.log"
    server_log_offset = (
        server_log.stat().st_size
        if profile == "zvec-grep" and server_log.is_file()
        else 0
    )

    command = [
        str(codex),
        "exec",
        "--json",
        "--ephemeral",
        "--model",
        model,
        "-c",
        f'model_reasoning_effort="{reasoning_effort}"',
        "-c",
        'web_search="disabled"',
        "-c",
        "allow_login_shell=false",
        "--sandbox",
        "workspace-write",
    ]
    command.extend(
        [
            "--ignore-rules",
            "--skip-git-repo-check",
            "-C",
            str(workspace),
            "-o",
            str(final_path),
            "-",
        ]
    )
    environment = dict(os.environ)
    environment.pop("ZVEC_GREP_HOME", None)
    environment.pop("ZVEC_GREP_SERVER_URL", None)
    environment.update(
        {
            "CODEX_HOME": str(codex_home),
            "HOME": str(codex_home),
            "GIT_CEILING_DIRECTORIES": str(workspace.parent),
            "NO_COLOR": "1",
            "CODEX_CI": "1",
        }
    )
    if profile == "zvec-grep":
        runtime_environment = server_environment(config, artifacts)
        environment["ZVEC_GREP_HOME"] = runtime_environment["ZVEC_GREP_HOME"]
        environment["ZVEC_GREP_SERVER_URL"] = runtime_environment[
            "ZVEC_GREP_SERVER_URL"
        ]
    command_record = {
        "args": command,
        "cwd": str(workspace),
        "profile": profile,
        "model": model,
        "reasoning_effort": reasoning_effort,
        "environment": {
            "CODEX_HOME": environment["CODEX_HOME"],
            "GIT_CEILING_DIRECTORIES": environment["GIT_CEILING_DIRECTORIES"],
            "PATH": environment.get("PATH", ""),
            **(
                {
                    "ZVEC_GREP_HOME": environment["ZVEC_GREP_HOME"],
                    "ZVEC_GREP_SERVER_URL": environment[
                        "ZVEC_GREP_SERVER_URL"
                    ],
                }
                if profile == "zvec-grep"
                else {}
            ),
        },
    }
    write_json(output_dir / "command.json", command_record)

    started_at = _iso_now()
    started = time.monotonic()
    idle_killed = False
    activity = [started]
    next_heartbeat = started + HEARTBEAT_SECONDS
    progress_width = _progress(
        f"→ case {query_id} · trial {trial_index} · {profile} · attempt {attempt}",
        0,
    )
    with events_path.open("wb") as events, stderr_path.open("wb") as errors:
        process = subprocess.Popen(
            command,
            cwd=workspace,
            env=environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        try:
            assert process.stdin and process.stdout and process.stderr
            stdout_thread = threading.Thread(
                target=_copy_stream,
                args=(process.stdout, events, activity),
                daemon=True,
            )
            stderr_thread = threading.Thread(
                target=_copy_stream,
                args=(process.stderr, errors, activity),
                daemon=True,
            )
            stdout_thread.start()
            stderr_thread.start()
            process.stdin.write(prompt.encode("utf-8"))
            process.stdin.close()
            while process.poll() is None:
                now = time.monotonic()
                if now >= next_heartbeat:
                    progress_width = _progress(
                        f"· case {query_id} · trial {trial_index} · {profile} · "
                        f"attempt {attempt} · "
                        f"elapsed {_elapsed(now - started)} · "
                        f"last activity {_elapsed(now - activity[0])} ago",
                        progress_width,
                    )
                    next_heartbeat = now + HEARTBEAT_SECONDS
                if (
                    idle_timeout_seconds
                    and now - activity[0] > idle_timeout_seconds
                ):
                    idle_killed = True
                    _terminate_group(process)
                    break
                time.sleep(1)
            exit_code = process.wait()
            stdout_thread.join(timeout=5)
            stderr_thread.join(timeout=5)
        except BaseException:
            _terminate_group(process)
            raise
        finally:
            _clear_progress(progress_width)

    finished_at = _iso_now()
    wall_seconds = time.monotonic() - started
    trace = parse_trace(events_path, final_path)
    stderr = stderr_path.read_text(encoding="utf-8", errors="replace")
    status, infrastructure_failure = _classify_attempt(
        exit_code=exit_code,
        idle_killed=idle_killed,
        trace=trace,
        stderr=stderr,
    )

    server_trace_path = output_dir / "zvec-server.jsonl"
    server_trace_written = False
    if profile == "zvec-grep" and server_log.is_file():
        with server_log.open("rb") as source:
            source.seek(min(server_log_offset, server_log.stat().st_size))
            server_trace = source.read().decode("utf-8", errors="replace")
        atomic_write_text(server_trace_path, server_trace)
        server_trace_written = True
    result = AttemptResult(
        query_id=query_id,
        profile=profile,
        trial_index=trial_index,
        status=status,
        attempt=attempt,
        started_at=started_at,
        finished_at=finished_at,
        wall_seconds=wall_seconds,
        exit_code=exit_code,
        infrastructure_failure=infrastructure_failure,
        trace=trace,
        paths={
            "events": str(events_path.resolve()),
            "stderr": str(stderr_path.resolve()),
            "response": str(final_path.resolve()),
            **(
                {"zvec_server": str(server_trace_path.resolve())}
                if server_trace_written
                else {}
            ),
        },
    )
    write_json(output_dir / "result.json", result.to_dict())
    write_json(
        output_dir / "usage.json", trace.usage.to_dict() if trace.usage else None
    )
    write_json(output_dir / "tools.json", [call.to_dict() for call in trace.tool_calls])
    write_json(output_dir / "documents.json", list(trace.observed_docids))
    return result
