from __future__ import annotations

import os
import shutil
import subprocess
import sys
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO, TextIO
from typing import Mapping, Sequence


@dataclass(frozen=True)
class CommandResult:
    args: tuple[str, ...]
    returncode: int
    stdout: str
    stderr: str

    @property
    def ok(self) -> bool:
        return self.returncode == 0


def resolve_executable(command: str) -> Path | None:
    candidate = Path(command).expanduser()
    if candidate.is_absolute() or candidate.parent != Path("."):
        return candidate.resolve() if candidate.is_file() else None
    resolved = shutil.which(command)
    if resolved:
        return Path(resolved).resolve()
    if command in {"codex", "rg"}:
        application_candidates = tuple(
            root / command
            for root in (
                Path("/Applications/ChatGPT.app/Contents/Resources"),
                Path("/Applications/Codex.app/Contents/Resources"),
                Path.home() / "Applications/ChatGPT.app/Contents/Resources",
                Path.home() / "Applications/Codex.app/Contents/Resources",
            )
        )
        for application_candidate in application_candidates:
            if application_candidate.is_file():
                return application_candidate.resolve()
    return None


def run_command(
    args: Sequence[str | Path],
    *,
    cwd: Path | None = None,
    env: Mapping[str, str] | None = None,
    timeout: float | None = None,
) -> CommandResult:
    command = tuple(str(value) for value in args)
    completed = subprocess.run(
        command,
        cwd=cwd,
        env=None if env is None else dict(env),
        check=False,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    return CommandResult(
        args=command,
        returncode=completed.returncode,
        stdout=completed.stdout,
        stderr=completed.stderr,
    )


def _write_console(destination: TextIO, chunk: bytes) -> None:
    binary = getattr(destination, "buffer", None)
    if binary is not None:
        binary.write(chunk)
        binary.flush()
        return
    destination.write(chunk.decode("utf-8", errors="replace"))
    destination.flush()


def _pump(
    source: BinaryIO,
    log: BinaryIO,
    console: TextIO,
    tail: bytearray,
    *,
    tail_bytes: int,
) -> None:
    read = getattr(source, "read1", source.read)
    while chunk := read(64 * 1024):
        log.write(chunk)
        log.flush()
        _write_console(console, chunk)
        tail.extend(chunk)
        if len(tail) > tail_bytes:
            del tail[:-tail_bytes]


def run_streaming_command(
    args: Sequence[str | Path],
    *,
    stdout_log: Path,
    stderr_log: Path,
    cwd: Path | None = None,
    env: Mapping[str, str] | None = None,
    tail_bytes: int = 64 * 1024,
) -> CommandResult:
    """Run a command while teeing both streams to the terminal and log files."""
    command = tuple(str(value) for value in args)
    stdout_log.parent.mkdir(parents=True, exist_ok=True)
    stderr_log.parent.mkdir(parents=True, exist_ok=True)
    stdout_tail = bytearray()
    stderr_tail = bytearray()
    with stdout_log.open("wb") as stdout_file, stderr_log.open("wb") as stderr_file:
        process = subprocess.Popen(
            command,
            cwd=cwd,
            env=None if env is None else dict(env),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        assert process.stdout and process.stderr
        stdout_thread = threading.Thread(
            target=_pump,
            args=(process.stdout, stdout_file, sys.stdout, stdout_tail),
            kwargs={"tail_bytes": tail_bytes},
            daemon=True,
        )
        stderr_thread = threading.Thread(
            target=_pump,
            args=(process.stderr, stderr_file, sys.stderr, stderr_tail),
            kwargs={"tail_bytes": tail_bytes},
            daemon=True,
        )
        stdout_thread.start()
        stderr_thread.start()
        try:
            returncode = process.wait()
        except KeyboardInterrupt:
            process.terminate()
            process.wait()
            raise
        finally:
            stdout_thread.join()
            stderr_thread.join()
    return CommandResult(
        args=command,
        returncode=returncode,
        stdout=stdout_tail.decode("utf-8", errors="replace"),
        stderr=stderr_tail.decode("utf-8", errors="replace"),
    )


def inherited_environment() -> dict[str, str]:
    return dict(os.environ)
