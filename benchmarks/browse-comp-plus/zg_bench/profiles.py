from __future__ import annotations

import os
import time
from pathlib import Path
from typing import Any

from .artifacts import (
    fingerprint,
    read_json,
    sha256_file,
    utc_now,
    write_json,
)
from .config import BenchmarkConfig
from .corpus import workspace_root
from .process import inherited_environment, resolve_executable, run_command

AGENTS_START = "<!-- ZVEC_GREP_START -->"


def server_url(config: BenchmarkConfig) -> str:
    return f"http://127.0.0.1:{config.zvec_grep.server_port}/mcp"


def server_environment(config: BenchmarkConfig, artifacts: Path) -> dict[str, str]:
    environment = inherited_environment()
    environment.update(
        {
            "ZVEC_GREP_HOME": str((artifacts / "runtime" / "zvec-home").resolve()),
            "ZVEC_GREP_MODEL_CACHE": str((artifacts / "models").resolve()),
            "ZVEC_GREP_SERVER_URL": server_url(config),
        }
    )
    return environment


def model_base_url() -> str:
    from urllib.parse import urlparse

    value = os.environ.get("BROWSE_COMP_MODEL_BASE_URL", "").strip().rstrip("/")
    parsed = urlparse(value)
    if not parsed.hostname or parsed.scheme not in {"https", "http"}:
        raise ValueError(
            "BROWSE_COMP_MODEL_BASE_URL must be an OpenAI-compatible base URL"
        )
    if parsed.username or parsed.password or parsed.query or parsed.fragment:
        raise ValueError(
            "model base URL must not contain credentials, query, or fragment"
        )
    if parsed.scheme != "https" and parsed.hostname not in {
        "127.0.0.1",
        "localhost",
        "::1",
    }:
        raise ValueError("model base URL requires HTTPS (except local test fixtures)")
    return value


def opencode_environment(home: Path) -> dict[str, str]:
    # Do not inherit user providers, agents, plugins, skills, or config overrides.
    allowed = {
        "PATH",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TZ",
        "TMPDIR",
        "TEMP",
        "TMP",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "NO_PROXY",
        "https_proxy",
        "http_proxy",
        "no_proxy",
        "OPENAI_API_KEY",
    }
    environment = {key: value for key, value in os.environ.items() if key in allowed}
    environment.update(
        {
            "HOME": str(home),
            "OPENCODE_CONFIG": str(home / "opencode.json"),
            "OPENCODE_CONFIG_DIR": str(home),
            "OPENCODE_DISABLE_PROJECT_CONFIG": "true",
            "OPENCODE_DISABLE_EXTERNAL_SKILLS": "true",
            "OPENCODE_DISABLE_CLAUDE_CODE": "true",
            "OPENCODE_DISABLE_DEFAULT_PLUGINS": "true",
            "OPENCODE_DISABLE_MODELS_FETCH": "true",
            "OPENCODE_DISABLE_AUTOUPDATE": "true",
            "XDG_CONFIG_HOME": str(home / "config"),
            "XDG_DATA_HOME": str(home / "data"),
            "XDG_STATE_HOME": str(home / "state"),
            "XDG_CACHE_HOME": str(home / "cache"),
            "NO_COLOR": "1",
        }
    )
    return environment


def opencode_configuration(
    config: BenchmarkConfig, *, judge: bool = False
) -> dict[str, Any]:
    provider, separator, model = config.run.model.partition("/")
    if provider != "custom-openai" or not separator or not model:
        raise ValueError("OpenCode model must use custom-openai/<model-id>")
    # Both profiles expose the same local tools; only Treatment adds zvec-grep.
    # No delegated sessions: all model usage belongs to this single session.
    permission = (
        {"*": "deny"}
        if judge
        else {
            "*": "allow",
            "websearch": "deny",
            "webfetch": "deny",
            "task": "deny",
            "external_directory": "deny",
            "question": "deny",
            "skill": "deny",
        }
    )
    agent = {
        "temperature": config.run.temperature,
        "options": {"seed": config.run.seed},
        "permission": permission.copy(),
    }
    return {
        "$schema": "https://opencode.ai/config.json",
        "model": config.run.model,
        "small_model": config.run.model,
        "enabled_providers": [provider],
        "provider": {
            provider: {
                "npm": "@ai-sdk/openai-compatible",
                "name": "BrowseComp-Plus Qwen",
                "options": {
                    "apiKey": "{env:OPENAI_API_KEY}",
                    "baseURL": model_base_url(),
                },
                "models": {
                    model: {
                        "name": "Qwen3.8 Max",
                        "temperature": True,
                        "reasoning": True,
                        "interleaved": {"field": "reasoning_content"},
                        "limit": {
                            "context": config.run.context_limit,
                            "output": config.run.max_output_tokens,
                        },
                        "options": {
                            "enable_thinking": True,
                            "reasoningEffort": config.run.reasoning_effort,
                        },
                    }
                },
            }
        },
        "agent": {
            name: agent.copy()
            for name in (
                "build",
                "plan",
                "general",
                "explore",
                "compaction",
                "title",
                "summary",
            )
        },
        "permission": permission,
        "instructions": [],
        "autoupdate": False,
        "share": "disabled",
        "snapshot": False,
        "lsp": False,
        "formatter": False,
        # No background compaction calls missing from the trial trace.
        "compaction": {"auto": False, "prune": False},
    }


def prepare_judge_home(config: BenchmarkConfig, artifacts: Path, home: Path) -> Path:
    del artifacts
    home.mkdir(parents=True, exist_ok=True, mode=0o700)
    home.chmod(0o700)
    write_json(home / "opencode.json", opencode_configuration(config, judge=True))
    return home


def prepare_profiles(
    config: BenchmarkConfig,
    artifacts: Path,
    *,
    opencode_bin: str = "opencode",
    profiles_root: Path,
    manifest_path: Path,
) -> Path:
    started = time.monotonic()
    opencode = resolve_executable(opencode_bin)
    zg = resolve_executable("zg")
    if opencode is None or zg is None:
        raise RuntimeError("OpenCode and zvec-grep must be available")
    if not os.environ.get("OPENAI_API_KEY"):
        raise RuntimeError("OPENAI_API_KEY is required for the Qwen provider")
    baseline = profiles_root / "baseline" / "opencode-home"
    treatment = profiles_root / "zvec-grep" / "opencode-home"
    for home in (baseline, treatment):
        home.mkdir(parents=True, exist_ok=True, mode=0o700)
        home.chmod(0o700)
        write_json(home / "opencode.json", opencode_configuration(config))
        (home / "AGENTS.md").unlink(missing_ok=True)
    environment = opencode_environment(treatment)
    runtime = server_environment(config, artifacts)
    environment.update(
        {name: runtime[name] for name in ("ZVEC_GREP_HOME", "ZVEC_GREP_SERVER_URL")}
    )
    install_started = time.monotonic()
    install = run_command(
        [
            zg,
            "--install",
            "--target",
            "opencode",
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
    baseline_config = read_json(baseline / "opencode.json")
    treatment_config = read_json(treatment / "opencode.json")
    guidance = treatment / "AGENTS.md"
    if baseline_config.get("mcp") or (baseline / "AGENTS.md").exists():
        raise RuntimeError("baseline profile contains zvec-grep integration")
    if not guidance.is_file() or AGENTS_START not in guidance.read_text():
        raise RuntimeError("treatment profile is missing zvec-grep guidance")
    expected_mcp = {
        "type": "remote",
        "url": server_url(config),
        "enabled": True,
        "timeout": config.zvec_grep.mcp_tool_timeout_seconds * 1000,
        "oauth": False,
    }
    if treatment_config.get("mcp", {}).get("zvec_grep") != expected_mcp:
        raise RuntimeError("treatment MCP configuration does not match the benchmark")
    # Explicitly include only installed guidance, even with project rules disabled.
    treatment_config["instructions"] = [str(guidance.resolve())]
    write_json(treatment / "opencode.json", treatment_config)
    build = zvec_grep_build_identity(zg)
    files = {
        "baseline_config_sha256": sha256_file(baseline / "opencode.json"),
        "baseline_agents_sha256": None,
        "treatment_config_sha256": sha256_file(treatment / "opencode.json"),
        "treatment_agents_sha256": sha256_file(guidance),
    }
    manifest = {
        "stage": "profiles",
        "generated_at": utc_now(),
        "opencode_bin": str(opencode),
        "baseline_home": str(baseline.resolve()),
        "treatment_home": str(treatment.resolve()),
        "zvec_grep_home": environment["ZVEC_GREP_HOME"],
        "zvec_grep_server_url": environment["ZVEC_GREP_SERVER_URL"],
        "zvec_grep_build": build,
        "authentication": {
            "baseline": "provider API key",
            "zvec-grep": "provider API key",
        },
        "files": files,
        "fingerprint": fingerprint(
            [build["fingerprint"], *(files[key] or "<absent>" for key in sorted(files))]
        ),
        "baseline": {"zvec_mcp": False, "zvec_guidance": False},
        "zvec-grep": {
            "zvec_mcp": True,
            "zvec_guidance": True,
            "install_command": "zg --install --target opencode --mcp-transport http --yes",
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
        "baseline_config_sha256": sha256_file(baseline / "opencode.json"),
        "baseline_agents_sha256": (
            sha256_file(baseline_agents) if baseline_agents.is_file() else None
        ),
        "treatment_config_sha256": sha256_file(treatment / "opencode.json"),
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
