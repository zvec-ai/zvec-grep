from __future__ import annotations

import platform
import sys
from dataclasses import asdict, dataclass
from typing import Any

from .artifacts import utc_now
from .config import BenchmarkConfig
from .process import resolve_executable, run_command


@dataclass(frozen=True)
class Check:
    name: str
    ok: bool
    detail: str

    @property
    def status(self) -> str:
        return "pass" if self.ok else "fail"

    def to_dict(self) -> dict[str, Any]:
        return {**asdict(self), "status": self.status}


def _command(name: str, executable: str, args: list[str]) -> Check:
    resolved = resolve_executable(executable)
    if resolved is None:
        return Check(name, False, f"{executable!r} was not found")
    result = run_command([resolved, *args], timeout=30)
    output = (result.stdout or result.stderr).strip().splitlines()
    detail = output[0] if output else f"exit {result.returncode}"
    return Check(name, result.ok, f"{detail} ({resolved})")


def _authentication(codex_bin: str) -> Check:
    executable = resolve_executable(codex_bin)
    if executable is None:
        return Check("Codex authentication", False, "Codex was not found")
    result = run_command([executable, "login", "status"], timeout=30)
    status = next(
        (
            line.strip()
            for line in (*result.stdout.splitlines(), *result.stderr.splitlines())
            if line.lower().startswith("logged in")
        ),
        "authenticated",
    )
    return Check(
        "Codex authentication",
        result.ok,
        status if result.ok else "run 'codex login' before the benchmark",
    )


def _zvec_version() -> Check:
    executable = resolve_executable("zg")
    if executable is None:
        return Check("zvec-grep", False, "'zg' was not found")
    result = run_command([executable, "--version"], timeout=30)
    value = result.stdout.strip()
    return Check(
        "zvec-grep",
        result.ok and bool(value),
        f"{value or 'unknown'} ({executable})",
    )


def run_doctor(
    config: BenchmarkConfig,
    *,
    codex_bin: str = "codex",
) -> dict[str, Any]:
    version = sys.version_info
    system = platform.system()
    checks = [
        Check(
            "Python",
            (3, 12) <= version[:2] < (3, 14),
            f"{version.major}.{version.minor}.{version.micro}; requires >=3.12,<3.14",
        ),
        Check(
            "Platform",
            system in {"Darwin", "Linux"},
            f"{system} {platform.machine()}; native macOS and Linux are supported",
        ),
        _zvec_version(),
        _command("Codex", codex_bin, ["--version"]),
        _authentication(codex_bin),
    ]
    report = {
        "stage": "doctor",
        "generated_at": utc_now(),
        "ready": all(check.ok for check in checks),
        "configuration": str(config.path),
        "checks": [check.to_dict() for check in checks],
    }
    return report


def _styled(value: str, color: str, *, enabled: bool) -> str:
    return f"\033[{color}m{value}\033[0m" if enabled else value


def format_report(report: dict[str, Any], *, color: bool = False) -> str:
    lines = ["BrowseComp-Plus doctor", ""]
    for check in report["checks"]:
        status = str(check["status"]).upper()
        status_color = "32" if check["ok"] else "31"
        label = _styled(f"[{status}]", status_color, enabled=color)
        lines.append(f"{label} {check['name']}: {check['detail']}")
    result = "ready" if report["ready"] else "not ready"
    result_color = "32" if report["ready"] else "31"
    lines.extend(
        (
            "",
            f"Result: {_styled(result, result_color, enabled=color)}",
        )
    )
    return "\n".join(lines)
