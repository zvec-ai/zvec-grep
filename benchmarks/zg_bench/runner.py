from __future__ import annotations

import json
import os
import re
import subprocess
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Literal, Sequence

import yaml

from .settings import (
    AGENT_SETUP_TIMEOUT_MULTIPLIER,
    CODEX_VERSION,
    OPENCODE_ALIYUN_GLM_BASE_URL,
    OPENCODE_ALIYUN_GLM_MODEL,
    OPENCODE_VERSION,
    QWEN_CODE_DASHSCOPE_BASE_URL,
    QWEN_CODE_DASHSCOPE_MODEL,
    QWEN_CODE_VERSION,
    ZVEC_GREP_API_KEY_ENV_VARS,
    ZVEC_GREP_BINDING_PACKAGE,
    ZVEC_GREP_EMBEDDING,
    ZVEC_GREP_PACKAGE,
)

BENCHMARKS_DIR = Path(__file__).resolve().parents[1]
SUITES_DIR = BENCHMARKS_DIR / "suites"
DEFAULT_RUNS_DIR = BENCHMARKS_DIR / "runs"
ZVEC_GREP_SKILL_DIR = BENCHMARKS_DIR / "skills" / "zvec-grep"
ZVEC_CODEX_IMPORT_PATH = "zg_bench.agents.zvec_codex:ZvecCodex"
ZVEC_QWEN_CODE_IMPORT_PATH = (
    "zg_bench.agents.zvec_qwen_code:ZvecQwenCode"
)
ZVEC_OPENCODE_IMPORT_PATH = "zg_bench.agents.zvec_opencode:ZvecOpenCode"
SETUP_CACHE_DIR = BENCHMARKS_DIR / ".cache" / "agent-setup"
_SETUP_CACHE_TARGET = "/root/.nvm"
_CODEX_AGENT = "codex"
_QWEN_CODE_AGENT = "qwen-coder"
_OPENCODE_AGENT = "opencode"
_CACHEABLE_AGENTS = (_CODEX_AGENT, _QWEN_CODE_AGENT, _OPENCODE_AGENT)
_ZVEC_AGENT_IMPORT_PATHS = {
    _CODEX_AGENT: ZVEC_CODEX_IMPORT_PATH,
    _QWEN_CODE_AGENT: ZVEC_QWEN_CODE_IMPORT_PATH,
    _OPENCODE_AGENT: ZVEC_OPENCODE_IMPORT_PATH,
}
_QWEN_CODE_API_KEY_ENV_VARS = (
    "DASHSCOPE_API_KEY",
    "QWEN_API_KEY",
    "OPENAI_API_KEY",
)
_OPENCODE_ALIYUN_API_KEY_ENV_VARS = (
    "DASHSCOPE_API_KEY",
    "OPENAI_API_KEY",
)

Profile = Literal["baseline", "zvec-grep"]
ProfileSelection = Literal["baseline", "zvec-grep", "all"]
PROFILES: tuple[Profile, ...] = ("baseline", "zvec-grep")
PROFILE_SELECTIONS: tuple[ProfileSelection, ...] = (*PROFILES, "all")


class SuiteConfigError(ValueError):
    """Raised when a benchmark suite definition is invalid."""


@dataclass(frozen=True)
class SmokeSuite:
    name: str
    dataset: str
    task: str


def available_suites() -> list[str]:
    return sorted(path.stem for path in SUITES_DIR.glob("*.yaml"))


def _require_mapping(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise SuiteConfigError(f"{label} must be a mapping")
    return value


def _require_nonempty_string(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise SuiteConfigError(f"{label} must be a non-empty string")
    return value


def load_suite(name_or_path: str | Path) -> SmokeSuite:
    candidate = Path(name_or_path)
    if candidate.suffix in {".yaml", ".yml"}:
        path = candidate
    else:
        if candidate.name != str(candidate):
            raise SuiteConfigError(f"invalid suite name: {name_or_path}")
        path = SUITES_DIR / f"{candidate.name}.yaml"

    if not path.is_file():
        raise SuiteConfigError(f"suite definition not found: {path}")

    try:
        raw = yaml.safe_load(path.read_text(encoding="utf-8"))
    except yaml.YAMLError as error:
        raise SuiteConfigError(f"invalid YAML in {path}: {error}") from error

    root = _require_mapping(raw, "suite definition")
    name = _require_nonempty_string(root.get("name"), "name")
    dataset = _require_nonempty_string(root.get("dataset"), "dataset")
    if "@" not in dataset or dataset.endswith("@latest"):
        raise SuiteConfigError("dataset must use a pinned Harbor revision")

    tiers = _require_mapping(root.get("tiers"), "tiers")
    smoke = _require_mapping(tiers.get("smoke"), "tiers.smoke")
    tasks = smoke.get("tasks")
    if not isinstance(tasks, list) or len(tasks) != 1:
        raise SuiteConfigError("the smoke tier must contain exactly one task")
    task = _require_nonempty_string(tasks[0], "tiers.smoke.tasks[0]")

    if path.parent == SUITES_DIR and name != path.stem:
        raise SuiteConfigError(f"suite name {name!r} must match filename {path.stem!r}")

    return SmokeSuite(name=name, dataset=dataset, task=task)


def new_run_id() -> str:
    return datetime.now(UTC).strftime("%Y%m%d-%H%M%S")


def selected_profiles(selection: ProfileSelection) -> tuple[Profile, ...]:
    if selection == "all":
        return PROFILES
    if selection not in PROFILES:
        raise ValueError(f"unsupported profile: {selection}")
    return (selection,)


def _is_qwen_code_dashscope_model(agent: str, model: str) -> bool:
    return agent == _QWEN_CODE_AGENT and model in {
        QWEN_CODE_DASHSCOPE_MODEL,
        f"dashscope/{QWEN_CODE_DASHSCOPE_MODEL}",
        "qwen-3.7-max",
        "dashscope/qwen-3.7-max",
    }


def _is_opencode_aliyun_glm_model(agent: str, model: str) -> bool:
    return agent == _OPENCODE_AGENT and model in {
        OPENCODE_ALIYUN_GLM_MODEL,
        f"openai/{OPENCODE_ALIYUN_GLM_MODEL}",
    }


def _first_nonempty_env(names: Sequence[str]) -> tuple[str, str] | None:
    for name in names:
        value = os.environ.get(name, "").strip()
        if value:
            return name, value
    return None


def validate_profile_credentials(
    profiles: Sequence[Profile], *, agent: str, model: str
) -> None:
    if _is_qwen_code_dashscope_model(agent, model):
        if _first_nonempty_env(_QWEN_CODE_API_KEY_ENV_VARS) is None:
            accepted = ", ".join(_QWEN_CODE_API_KEY_ENV_VARS)
            raise ValueError(
                f"{QWEN_CODE_DASHSCOPE_MODEL} requires a DashScope API key; "
                f"export one of: {accepted}"
            )

    if _is_opencode_aliyun_glm_model(agent, model):
        if _first_nonempty_env(_OPENCODE_ALIYUN_API_KEY_ENV_VARS) is None:
            accepted = ", ".join(_OPENCODE_ALIYUN_API_KEY_ENV_VARS)
            raise ValueError(
                f"{OPENCODE_ALIYUN_GLM_MODEL} requires a DashScope API key; "
                f"export one of: {accepted}"
            )

    if "zvec-grep" not in profiles or not ZVEC_GREP_EMBEDDING.startswith("qwen/"):
        return
    if _first_nonempty_env(ZVEC_GREP_API_KEY_ENV_VARS) is not None:
        return
    accepted = ", ".join(ZVEC_GREP_API_KEY_ENV_VARS)
    raise ValueError(
        "the zvec-grep profile requires a Qwen embedding API key; "
        f"export one of: {accepted}"
    )


def execution_environment(*, agent: str, model: str) -> dict[str, str]:
    """Return Harbor's environment without placing credentials in its command."""
    environment = os.environ.copy()
    if _is_qwen_code_dashscope_model(agent, model):
        credential = _first_nonempty_env(_QWEN_CODE_API_KEY_ENV_VARS)
        if credential is not None:
            _, api_key = credential
            environment["OPENAI_API_KEY"] = api_key
    if _is_opencode_aliyun_glm_model(agent, model):
        credential = _first_nonempty_env(_OPENCODE_ALIYUN_API_KEY_ENV_VARS)
        if credential is not None:
            _, api_key = credential
            environment["OPENAI_API_KEY"] = api_key
        environment["OPENAI_BASE_URL"] = OPENCODE_ALIYUN_GLM_BASE_URL
    return environment


def default_job_name(suite: SmokeSuite, profile: Profile, *, run_id: str) -> str:
    return f"{run_id}-{suite.name}-smoke-{profile}"


def profile_job_name(
    suite: SmokeSuite,
    profile: Profile,
    *,
    run_id: str,
    override: str | None,
    paired: bool,
) -> str:
    if override:
        return f"{override}-{profile}" if paired else override
    return default_job_name(suite, profile, run_id=run_id)


def _cache_slug(value: str) -> str:
    return re.sub(r"[^A-Za-z0-9_.-]+", "-", value).strip("-.")


def _agent_version(agent: str) -> str:
    if agent == _CODEX_AGENT:
        return CODEX_VERSION
    if agent == _QWEN_CODE_AGENT:
        return QWEN_CODE_VERSION
    if agent == _OPENCODE_AGENT:
        return OPENCODE_VERSION
    raise ValueError(f"agent does not use a setup cache: {agent}")


def uses_setup_cache(agent: str) -> bool:
    return agent in _CACHEABLE_AGENTS


def setup_cache_volume_name(agent: str, profile: Profile) -> str:
    identity = f"{agent}-{_agent_version(agent)}-{profile}-linux-x64"
    if profile == "zvec-grep":
        identity += f"-{ZVEC_GREP_PACKAGE}-{ZVEC_GREP_BINDING_PACKAGE}"
    return f"zg-bench-{_cache_slug(identity)}"


def setup_cache_compose_path(agent: str, profile: Profile) -> Path:
    return SETUP_CACHE_DIR / f"{_cache_slug(agent)}-{profile}.compose.json"


def prepare_setup_cache(agent: str, profile: Profile) -> None:
    """Create the profile-isolated Docker volume and its Compose overlay."""
    volume_name = setup_cache_volume_name(agent, profile)
    SETUP_CACHE_DIR.mkdir(parents=True, exist_ok=True)
    overlay = {
        "services": {
            "main": {
                "platform": "linux/amd64",
                "volumes": [
                    {
                        "type": "volume",
                        "source": "agent-setup-cache",
                        "target": _SETUP_CACHE_TARGET,
                    }
                ]
            }
        },
        "volumes": {
            "agent-setup-cache": {
                "external": True,
                "name": volume_name,
            }
        },
    }
    setup_cache_compose_path(agent, profile).write_text(
        json.dumps(overlay, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )

    completed = subprocess.run(
        ["docker", "volume", "create", volume_name],
        check=False,
        capture_output=True,
        text=True,
    )
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout or "unknown error").strip()
        raise RuntimeError(f"could not prepare agent setup cache: {detail}")


def build_harbor_command(
    suite: SmokeSuite,
    *,
    profile: Profile,
    agent: str,
    model: str,
    jobs_dir: Path = DEFAULT_RUNS_DIR,
    job_name: str,
    harbor_executable: str = "harbor",
) -> list[str]:
    if profile not in PROFILES:
        raise ValueError(f"unsupported profile: {profile}")
    if not agent.strip():
        raise ValueError("agent must not be empty")
    if not model.strip():
        raise ValueError("model must not be empty")

    harbor_agent = agent
    harbor_model = model
    agent_kwargs: list[str] = []
    skills: list[str] = []

    if agent == _CODEX_AGENT:
        agent_kwargs.append(f"version={CODEX_VERSION}")
    elif agent == _QWEN_CODE_AGENT:
        agent_kwargs.append(f"version={QWEN_CODE_VERSION}")
        if _is_qwen_code_dashscope_model(agent, model):
            harbor_model = QWEN_CODE_DASHSCOPE_MODEL
            agent_kwargs.append(f"base_url={QWEN_CODE_DASHSCOPE_BASE_URL}")
    elif agent == _OPENCODE_AGENT:
        agent_kwargs.append(f"version={OPENCODE_VERSION}")
        if _is_opencode_aliyun_glm_model(agent, model):
            harbor_model = f"openai/{OPENCODE_ALIYUN_GLM_MODEL}"

    if profile == "zvec-grep":
        if agent not in _ZVEC_AGENT_IMPORT_PATHS:
            supported = ", ".join(_ZVEC_AGENT_IMPORT_PATHS)
            raise ValueError(
                "the zvec-grep profile currently supports --agent "
                f"{supported}"
            )
        harbor_agent = _ZVEC_AGENT_IMPORT_PATHS[agent]
        agent_kwargs.extend(
            [
                f"zvec_grep_package={ZVEC_GREP_PACKAGE}",
                f"zvec_binding_package={ZVEC_GREP_BINDING_PACKAGE}",
                f"embedding_model={ZVEC_GREP_EMBEDDING}",
            ]
        )
        if agent in {_CODEX_AGENT, _OPENCODE_AGENT}:
            agent_kwargs.append(f"mcp_target={agent}")
        else:
            if not ZVEC_GREP_SKILL_DIR.is_dir():
                raise ValueError(
                    f"zvec-grep skill not found: {ZVEC_GREP_SKILL_DIR}"
                )
            skills.append(str(ZVEC_GREP_SKILL_DIR.resolve()))

    command = [
        harbor_executable,
        "run",
        "--dataset",
        suite.dataset,
        "--include-task-name",
        suite.task,
        "--agent",
        harbor_agent,
        "--model",
        harbor_model,
        "--env",
        "docker",
        "--n-attempts",
        "1",
        "--n-concurrent",
        "1",
        "--agent-setup-timeout-multiplier",
        AGENT_SETUP_TIMEOUT_MULTIPLIER,
        "--jobs-dir",
        str(jobs_dir.resolve()),
        "--job-name",
        job_name,
    ]

    if uses_setup_cache(agent):
        command.extend(
            [
                "--extra-docker-compose",
                str(setup_cache_compose_path(agent, profile).resolve()),
                "--yes",
            ]
        )

    for agent_kwarg in agent_kwargs:
        command.extend(["--agent-kwarg", agent_kwarg])
    for skill in skills:
        command.extend(["--skill", skill])

    return command


def execute(
    command: Sequence[str], *, jobs_dir: Path, environment: dict[str, str] | None = None
) -> int:
    jobs_dir.mkdir(parents=True, exist_ok=True)
    completed = subprocess.run(
        command,
        cwd=BENCHMARKS_DIR,
        check=False,
        env=environment,
    )
    return completed.returncode
