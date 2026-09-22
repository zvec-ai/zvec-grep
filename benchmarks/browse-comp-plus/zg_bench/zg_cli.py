"""The zg command contract shared by execution and compatibility checks."""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path

from .process import run_command


@dataclass(frozen=True)
class Command:
    topic: str
    args: tuple[str, ...]
    optional: tuple[tuple[str, tuple[str, ...]], ...] = ()

    def build(self, executable: str | Path, **values: object) -> list[str]:
        args = list(self.args)
        for key, extra in self.optional:
            if values.get(key) is not None and values.get(key) is not False:
                args.extend(extra)
        return [str(executable), *(arg.format_map(values) for arg in args)]

    @property
    def flags(self) -> set[str]:
        args = self.args + tuple(
            arg for _, extra in self.optional for arg in extra
        )
        return {arg for arg in args if arg.startswith("--")}


INDEX = Command(
    "index",
    (
        "--index", "{root}", "--mode", "direct", "--embedding", "{embedding}",
        "--index-embedding-concurrency", "{concurrency}",
        "--max-filesize", "{max_filesize}", "--glob", "*.md",
    ),
    optional=(
        ("rebuild", ("--rebuild",)),
        ("device", ("--device", "{device}")),
    ),
)
STATUS = Command("status", ("--status", "{root}", "--mode", "direct", "--check-ready"))
AUTHORIZE = Command(
    "auth",
    (
        "--auth", "grant", "{root}", "--capability", "embedding",
        "--scope", "workspace", "--embedding", "{embedding}",
    ),
)
INSTALL = Command(
    "install",
    (
        "--install", "--target", "codex", "--mcp-transport", "http",
        "--mcp-tool-timeout", "{timeout}", "--yes",
    ),
)
SERVER_STATUS = Command("server", ("--server", "status", "--check-ready"))
SERVER_START = Command(
    "server", ("--server", "on", "--listen", "{listen}", "--mcp-toolset", "agent")
)
SERVER_STOP = Command("server", ("--server", "off"))
QUERY = Command(
    "search",
    (
        "{query}", "--mode", "server", "--refresh", "off",
        "--limit", "1", "--preview", "none",
    ),
)

COMMANDS = (
    INDEX, STATUS, AUTHORIZE, INSTALL,
    SERVER_STATUS, SERVER_START, SERVER_STOP, QUERY,
)


def compatibility_issues(executable: Path) -> list[str]:
    """Inspect help only: no indexing, credential changes, or server startup."""
    required: dict[str, set[str]] = {}
    for command in COMMANDS:
        required.setdefault(command.topic, set()).update(command.flags)
    issues = []
    for topic, flags in required.items():
        result = run_command([executable, "--help", topic], timeout=30)
        if not result.ok:
            issues.append(f"{topic}: could not read command help")
            continue
        advertised = set(re.findall(r"(?<![\w-])--[a-z][a-z0-9-]*", result.stdout))
        missing = flags - advertised
        if missing:
            issues.append(f"{topic}: missing {', '.join(sorted(missing))}")
    return issues
