from __future__ import annotations

import json
import os
import time
from pathlib import Path
from typing import Any

from .artifacts import (
    atomic_write_text,
    fingerprint,
    read_json,
    sha256_file,
    utc_now,
    write_json,
)
from .config import BENCHMARK_ROOT, BenchmarkConfig
from .corpus import workspace_root
from .process import inherited_environment, resolve_executable, run_command


CONFIG_START = "# ZVEC_GREP_START"
CONFIG_END = "# ZVEC_GREP_END"
AGENTS_START = "<!-- ZVEC_GREP_START -->"
AGENTS_END = "<!-- ZVEC_GREP_END -->"


def server_url(config: BenchmarkConfig) -> str:
    return f"http://127.0.0.1:{config.zvec_grep.server_port}/mcp"


def server_environment(
    config: BenchmarkConfig, artifacts: Path
) -> dict[str, str]:
    environment = inherited_environment()
    environment.update(
        {
            "ZVEC_GREP_HOME": str(
                (artifacts / "runtime" / "zvec-home").resolve()
            ),
            "ZVEC_GREP_SERVER_URL": server_url(config),
        }
    )
    return environment


def _link_if_present(source: Path, target: Path) -> None:
    if not source.exists():
        return
    if target.is_symlink() and target.resolve() == source.resolve():
        return
    if target.exists() or target.is_symlink():
        raise RuntimeError(f"refusing to replace profile path: {target}")
    target.symlink_to(source, target_is_directory=source.is_dir())


def _write_clean_config(path: Path, *, trusted_project: Path) -> None:
    atomic_write_text(
        path,
        "\n".join(
            (
                'web_search = "disabled"',
                'sandbox_mode = "workspace-write"',
                "allow_login_shell = false",
                "analytics.enabled = false",
                "feedback.enabled = false",
                'history.persistence = "none"',
                "",
                "[sandbox_workspace_write]",
                "network_access = true",
                "",
                "[features.network_proxy]",
                "enabled = true",
                "allow_local_binding = true",
                'domains = { "127.0.0.1" = "allow" }',
                "",
                f"[projects.{json.dumps(str(trusted_project))}]",
                'trust_level = "trusted"',
                "",
            )
        ),
    )


def _authentication_status(codex: Path, home: Path) -> str:
    environment = inherited_environment()
    environment.update(
        {
            "CODEX_HOME": str(home),
            "HOME": str(home),
            "NO_COLOR": "1",
        }
    )
    result = run_command(
        [codex, "login", "status"],
        env=environment,
        timeout=30,
    )
    if not result.ok:
        detail = result.stderr.strip() or result.stdout.strip()
        raise RuntimeError(
            f"Codex authentication is unavailable in profile {home}: "
            f"{detail or 'login status failed'}"
        )
    return next(
        (
            line.strip()
            for line in (*result.stdout.splitlines(), *result.stderr.splitlines())
            if line.lower().startswith("logged in")
        ),
        "authenticated",
    )


def prepare_profiles(
    config: BenchmarkConfig,
    artifacts: Path,
    *,
    codex_bin: str = "codex",
    source_codex_home: Path | None = None,
    profiles_root: Path,
    manifest_path: Path,
) -> Path:
    started = time.monotonic()
    codex = resolve_executable(codex_bin)
    zg = resolve_executable("zg")
    if codex is None:
        raise RuntimeError(f"Codex executable not found: {codex_bin}")
    if zg is None:
        raise RuntimeError("zvec-grep executable not found: zg")

    source_home = (
        (
            source_codex_home
            or Path(os.environ.get("CODEX_HOME", Path.home() / ".codex"))
        )
        .expanduser()
        .resolve()
    )
    root = profiles_root
    baseline = root / "baseline" / "codex-home"
    treatment = root / "zvec-grep" / "codex-home"
    trusted_project = BENCHMARK_ROOT.parents[1].resolve()
    for home in (baseline, treatment):
        home.mkdir(parents=True, exist_ok=True)
        _write_clean_config(
            home / "config.toml",
            trusted_project=trusted_project,
        )
        (home / "AGENTS.md").unlink(missing_ok=True)
        _link_if_present(source_home / "auth.json", home / "auth.json")
    environment = server_environment(config, artifacts)
    environment.update(
        {
            "CODEX_HOME": str(treatment),
            "HOME": str(treatment),
            "NO_COLOR": "1",
        }
    )
    install_started = time.monotonic()
    install = run_command(
        [
            zg,
            "--install",
            "--target",
            "codex",
            "--mcp-transport",
            "http",
            "--mcp-tool-timeout",
            str(config.zvec_grep.mcp_tool_timeout_seconds),
            "--yes",
        ],
        cwd=workspace_root(artifacts, "zvec-grep"),
        env=environment,
        timeout=180,
    )
    install_wall_seconds = time.monotonic() - install_started
    if not install.ok:
        raise RuntimeError(install.stderr.strip() or install.stdout.strip())

    baseline_config = (baseline / "config.toml").read_text(encoding="utf-8")
    baseline_agents = (
        (baseline / "AGENTS.md").read_text(encoding="utf-8")
        if (baseline / "AGENTS.md").is_file()
        else ""
    )
    treatment_config = (treatment / "config.toml").read_text(encoding="utf-8")
    treatment_agents = (
        (treatment / "AGENTS.md").read_text(encoding="utf-8")
        if (treatment / "AGENTS.md").is_file()
        else ""
    )
    if CONFIG_START in baseline_config or AGENTS_START in baseline_agents:
        raise RuntimeError("baseline profile contains zvec-grep integration")
    if CONFIG_START not in treatment_config or AGENTS_START not in treatment_agents:
        raise RuntimeError("treatment profile is missing zvec-grep integration")

    authentication = {
        "baseline": _authentication_status(codex, baseline),
        "zvec-grep": _authentication_status(codex, treatment),
    }
    baseline_config_path = baseline / "config.toml"
    treatment_config_path = treatment / "config.toml"
    treatment_agents_path = treatment / "AGENTS.md"
    build = zvec_grep_build_identity(zg)
    files: dict[str, str | None] = {
        "baseline_config_sha256": sha256_file(baseline_config_path),
        "baseline_agents_sha256": None,
        "treatment_config_sha256": sha256_file(treatment_config_path),
        "treatment_agents_sha256": sha256_file(treatment_agents_path),
    }
    profile_fingerprint = fingerprint(
        [
            build["fingerprint"],
            *(files[key] or "<absent>" for key in sorted(files)),
        ]
    )
    manifest = {
        "stage": "profiles",
        "generated_at": utc_now(),
        "codex_bin": str(codex),
        "source_codex_home": str(source_home),
        "baseline_home": str(baseline.resolve()),
        "treatment_home": str(treatment.resolve()),
        "zvec_grep_home": environment["ZVEC_GREP_HOME"],
        "zvec_grep_server_url": environment["ZVEC_GREP_SERVER_URL"],
        "zvec_grep_build": build,
        "authentication": authentication,
        "files": files,
        "fingerprint": profile_fingerprint,
        "baseline": {"zvec_mcp": False, "zvec_guidance": False},
        "zvec-grep": {
            "zvec_mcp": True,
            "zvec_guidance": True,
            "install_command": (
                "zg --install --target codex --mcp-transport http --yes"
            ),
        },
        "install_stdout": install.stdout,
        "install_stderr": install.stderr,
        "install_wall_seconds": install_wall_seconds,
        "preparation_wall_seconds": time.monotonic() - started,
    }
    write_json(manifest_path, manifest)
    return manifest_path


def zvec_grep_build_identity(executable: Path) -> dict[str, str]:
    resolved = executable.resolve()
    package_root = _package_root(resolved)
    build_root = package_root / "dist" if package_root else resolved.parent
    files = sorted(path for path in build_root.rglob("*") if path.is_file())
    if not files:
        files = [resolved]
    build_fingerprint = fingerprint(
        value
        for path in files
        for value in (
            str(path.relative_to(build_root)),
            sha256_file(path),
        )
    )
    return {
        "executable": str(resolved),
        "build_root": str(build_root.resolve()),
        "fingerprint": build_fingerprint,
    }


def validate_profiles(manifest_path: Path) -> dict[str, Any]:
    executable = resolve_executable("zg")
    if executable is None:
        raise RuntimeError("zvec-grep executable not found: zg")
    manifest = read_json(manifest_path)
    expected_build = manifest.get("zvec_grep_build", {})
    actual_build = zvec_grep_build_identity(executable)
    if expected_build.get("fingerprint") != actual_build["fingerprint"]:
        raise RuntimeError(
            "zvec-grep build changed after this run was created; "
            "resume with the original executable or start a new run"
        )
    baseline = Path(manifest["baseline_home"])
    treatment = Path(manifest["treatment_home"])
    baseline_agents = baseline / "AGENTS.md"
    actual_files: dict[str, str | None] = {
        "baseline_config_sha256": sha256_file(baseline / "config.toml"),
        "baseline_agents_sha256": (
            sha256_file(baseline_agents) if baseline_agents.is_file() else None
        ),
        "treatment_config_sha256": sha256_file(treatment / "config.toml"),
        "treatment_agents_sha256": sha256_file(treatment / "AGENTS.md"),
    }
    if manifest.get("files") != actual_files:
        raise RuntimeError(
            "benchmark profile files changed after this run was created; "
            "restore the run artifacts or start a new run"
        )
    expected_fingerprint = fingerprint(
        [
            actual_build["fingerprint"],
            *(actual_files[key] or "<absent>" for key in sorted(actual_files)),
        ]
    )
    if manifest.get("fingerprint") != expected_fingerprint:
        raise RuntimeError("benchmark profile fingerprint is invalid")
    return manifest


def _package_root(executable: Path) -> Path | None:
    for parent in executable.parents:
        if (parent / "package.json").is_file() and (parent / "dist").is_dir():
            return parent
    return None


def ensure_server(
    config: BenchmarkConfig, artifacts: Path, *, restart: bool = False
) -> None:
    executable = resolve_executable("zg")
    if executable is None:
        raise RuntimeError("zvec-grep executable not found: zg")
    environment = server_environment(config, artifacts)
    check = run_command(
        [executable, "--server", "status", "--check-ready"],
        env=environment,
        timeout=30,
    )
    if check.ok and not restart:
        return
    if check.ok:
        stop = run_command(
            [executable, "--server", "off"],
            env=environment,
            timeout=60,
        )
        if not stop.ok:
            raise RuntimeError(stop.stderr.strip() or stop.stdout.strip())
    start = run_command(
        [
            executable,
            "--server",
            "on",
            "--listen",
            f"127.0.0.1:{config.zvec_grep.server_port}",
            "--mcp-toolset",
            "agent",
        ],
        env=environment,
        timeout=60,
    )
    if not start.ok:
        raise RuntimeError(start.stderr.strip() or start.stdout.strip())
    check = run_command(
        [executable, "--server", "status", "--check-ready"],
        env=environment,
        timeout=30,
    )
    if not check.ok:
        raise RuntimeError(check.stderr.strip() or check.stdout.strip())


def stop_server(config: BenchmarkConfig, artifacts: Path) -> None:
    executable = resolve_executable("zg")
    if executable is None:
        raise RuntimeError("zvec-grep executable not found: zg")
    result = run_command(
        [executable, "--server", "off"],
        env=server_environment(config, artifacts),
        timeout=60,
    )
    if not result.ok:
        raise RuntimeError(result.stderr.strip() or result.stdout.strip())


def prepare_search_runtime(
    config: BenchmarkConfig,
    artifacts: Path,
    *,
    restart_server: bool = False,
) -> dict[str, object]:
    """Verify the daemon and warm the existing index outside measured agent time."""
    executable = resolve_executable("zg")
    if executable is None:
        raise RuntimeError("zvec-grep executable not found: zg")
    started = time.monotonic()
    ensure_server(
        config,
        artifacts,
        restart=restart_server,
    )
    root = workspace_root(artifacts, "zvec-grep")
    environment = server_environment(config, artifacts)
    warmup = run_command(
        [
            executable,
            "benchmark runtime readiness",
            "--mode",
            "server",
            "--refresh",
            "off",
            "--limit",
            "1",
            "--preview",
            "none",
        ],
        cwd=root,
        env=environment,
        timeout=max(900, config.zvec_grep.mcp_tool_timeout_seconds),
    )
    if not warmup.ok:
        raise RuntimeError(warmup.stderr.strip() or warmup.stdout.strip())
    warmup_wall_seconds = time.monotonic() - started
    result: dict[str, object] = {
        "warmup_wall_seconds": warmup_wall_seconds,
        "root": str(root),
        "server_url": environment["ZVEC_GREP_SERVER_URL"],
        "warmup_stdout": warmup.stdout,
        "warmup_stderr": warmup.stderr,
    }
    return result
