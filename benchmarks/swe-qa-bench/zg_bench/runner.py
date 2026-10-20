from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import subprocess
import tempfile
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Literal, Sequence

import yaml

from .engines.opencode.config import build_opencode_config
from .engines.registry import (
    AGENT_MODEL_SUPPORT as AGENT_MODEL_SUPPORT,
)
from .engines.registry import (
    CLAUDE_CODE_CREDENTIAL_ENV_VARS as _CLAUDE_CODE_CREDENTIAL_ENV_VARS,
)
from .engines.registry import (
    AgentModelSupport as AgentModelSupport,
)
from .engines.registry import (
    available_agent_models as available_agent_models,
)
from .engines.registry import (
    execution_environment as execution_environment,
)
from .engines.registry import (
    first_nonempty_env as _first_nonempty_env,
)
from .engines.registry import (
    resolve_agent_model,
    resolve_opencode_model,
)
from .engines.registry import (
    validate_profile_credentials as validate_profile_credentials,
)
from .settings import (
    AGENT_SETUP_TIMEOUT_MULTIPLIER,
    CLAUDE_CODE_MAX_BUDGET_USD,
    CLAUDE_CODE_REASONING_EFFORT,
    CLAUDE_CODE_VERSION,
    CODEX_VERSION,
    OPENCODE_VERSION,
    ZVEC_GREP_BINDING_PACKAGE,
    ZVEC_GREP_EMBEDDING,
    ZVEC_GREP_EMBEDDING_ENDPOINT,
    ZVEC_GREP_INDEX_SEED_ENV,
    ZVEC_GREP_PACKAGE,
    resolve_zvec_grep_index_seed_dir,
)

BENCHMARKS_DIR = Path(__file__).resolve().parents[1]
SUITES_DIR = BENCHMARKS_DIR / "suites"
DEFAULT_RUNS_DIR = BENCHMARKS_DIR / "runs"
OPENCODE_IMPORT_PATH = "zg_bench.agents.opencode:ResilientOpenCode"
ZVEC_CLAUDE_CODE_IMPORT_PATH = "zg_bench.agents.zvec_claude_code:ZvecClaudeCode"
ZVEC_CODEX_IMPORT_PATH = "zg_bench.agents.zvec_codex:ZvecCodex"
ZVEC_OPENCODE_IMPORT_PATH = "zg_bench.agents.zvec_opencode:ZvecOpenCode"
SETUP_CACHE_DIR = BENCHMARKS_DIR / ".cache" / "agent-setup"
LOCAL_PACKAGE_DIR = SETUP_CACHE_DIR / "local-package"
LOCAL_NPM_CACHE_DIR = SETUP_CACHE_DIR / "npm-cache"
LOCAL_ZVEC_GREP_PACKAGE_TARGET = "/tmp/zg-bench-zvec-grep.tgz"
_SETUP_CACHE_TARGET = "/root/.nvm"
_CLAUDE_CODE_INSTALL_CACHE_TARGET = "/root/.local"
_CLAUDE_CODE_INSTALL_CACHE_SOURCE = "claude-code-install-cache"
_CODEX_AGENT = "codex"
_CLAUDE_CODE_AGENT = "claude-code"
_OPENCODE_AGENT = "opencode"
_CACHEABLE_AGENTS = (_CLAUDE_CODE_AGENT, _CODEX_AGENT, _OPENCODE_AGENT)
_ZVEC_AGENT_IMPORT_PATHS = {
    _CLAUDE_CODE_AGENT: ZVEC_CLAUDE_CODE_IMPORT_PATH,
    _CODEX_AGENT: ZVEC_CODEX_IMPORT_PATH,
    _OPENCODE_AGENT: ZVEC_OPENCODE_IMPORT_PATH,
}

Profile = Literal["baseline", "zvec-grep"]
ProfileSelection = Literal["baseline", "zvec-grep", "all"]
Tier = Literal["smoke", "ci", "full"]
PROFILES: tuple[Profile, ...] = ("baseline", "zvec-grep")
PROFILE_SELECTIONS: tuple[ProfileSelection, ...] = (*PROFILES, "all")
TIERS: tuple[Tier, ...] = ("smoke", "ci", "full")


class SuiteConfigError(ValueError):
    """Raised when a benchmark suite definition is invalid."""


@dataclass(frozen=True)
class BenchmarkSuite:
    name: str
    dataset: str | None
    tier: Tier
    tasks: tuple[str, ...] | None
    path: Path | None = None
    index_ignore_file: Path | None = None


@dataclass(frozen=True)
class PreparedSetupCache:
    compose_path: Path
    zvec_grep_package: str | None = None
    zvec_grep_package_sha256: str | None = None


@dataclass(frozen=True)
class PreparedZvecGrepPackage:
    install_spec: str
    bind_source: Path | None = None
    sha256: str | None = None


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


def load_suite(
    name_or_path: str | Path,
    *,
    tier: Tier = "smoke",
    task_overrides: Sequence[str] | None = None,
) -> BenchmarkSuite:
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
    raw_dataset = root.get("dataset")
    raw_path = root.get("path")
    if (raw_dataset is None) == (raw_path is None):
        raise SuiteConfigError("define exactly one of dataset or path")
    dataset: str | None = None
    local_path: Path | None = None
    if raw_dataset is not None:
        dataset = _require_nonempty_string(raw_dataset, "dataset")
        if "@" not in dataset or dataset.endswith("@latest"):
            raise SuiteConfigError("dataset must use a pinned Harbor revision")
    else:
        path_value = _require_nonempty_string(raw_path, "path")
        local_path = (path.parent / path_value).resolve()
        if not local_path.is_dir():
            raise SuiteConfigError(f"local Harbor dataset not found: {local_path}")

    tiers = _require_mapping(root.get("tiers"), "tiers")
    if tier not in TIERS:
        raise SuiteConfigError(f"unsupported tier: {tier}")
    if tier not in tiers:
        available = ", ".join(name for name in TIERS if name in tiers) or "none"
        raise SuiteConfigError(
            f"tier {tier!r} is not configured for {name!r}; available: {available}"
        )
    selected = _require_mapping(tiers.get(tier), f"tiers.{tier}")
    run_all = selected.get("all", False)
    raw_tasks = selected.get("tasks")
    if not isinstance(run_all, bool):
        raise SuiteConfigError(f"tiers.{tier}.all must be a boolean")
    if run_all and raw_tasks is not None:
        raise SuiteConfigError(f"tiers.{tier} cannot define both all: true and tasks")
    if run_all:
        tasks: tuple[str, ...] | None = None
    else:
        if not isinstance(raw_tasks, list) or not raw_tasks:
            raise SuiteConfigError(f"tiers.{tier} must contain tasks or set all: true")
        tasks = tuple(
            _require_nonempty_string(task, f"tiers.{tier}.tasks[{index}]")
            for index, task in enumerate(raw_tasks)
        )
        if len(set(tasks)) != len(tasks):
            raise SuiteConfigError(f"tiers.{tier}.tasks must not contain duplicates")
    if task_overrides:
        tasks = tuple(
            _require_nonempty_string(task, f"task override {index}")
            for index, task in enumerate(task_overrides)
        )
        if len(set(tasks)) != len(tasks):
            raise SuiteConfigError("task overrides must not contain duplicates")

    if path.parent == SUITES_DIR and name != path.stem:
        raise SuiteConfigError(f"suite name {name!r} must match filename {path.stem!r}")

    raw_index_ignore_file = root.get("index_ignore_file")
    index_ignore_file: Path | None = None
    if raw_index_ignore_file is not None:
        ignore_value = _require_nonempty_string(
            raw_index_ignore_file, "index_ignore_file"
        )
        index_ignore_file = (path.parent / ignore_value).resolve()
        if not index_ignore_file.is_file():
            raise SuiteConfigError(
                "index_ignore_file must reference an existing file: "
                f"{index_ignore_file}"
            )

    return BenchmarkSuite(
        name=name,
        dataset=dataset,
        path=local_path,
        tier=tier,
        tasks=tasks,
        index_ignore_file=index_ignore_file,
    )


def new_run_id() -> str:
    return datetime.now(UTC).strftime("%Y%m%d-%H%M%S")


def selected_profiles(selection: ProfileSelection) -> tuple[Profile, ...]:
    if selection == "all":
        return PROFILES
    if selection not in PROFILES:
        raise ValueError(f"unsupported profile: {selection}")
    return (selection,)


def validate_zvec_grep_package_compatibility(
    profiles: Sequence[Profile], *, agent: str, zvec_grep_package: str
) -> None:
    if "zvec-grep" not in profiles:
        return

    normalized = normalize_zvec_grep_package(zvec_grep_package)
    candidate = Path(normalized).expanduser()
    if candidate.exists():
        if candidate.is_dir() or (candidate.is_file() and candidate.suffix == ".tgz"):
            return
        raise ValueError(
            "local zvec-grep package must be a directory or .tgz file: " f"{candidate}"
        )
    if _looks_like_package_path(normalized):
        raise ValueError(f"local zvec-grep package does not exist: {candidate}")
    match = re.fullmatch(
        r"@zvec/zvec-grep@v?(\d+)\.(\d+)\.(\d+)(?:[-+][0-9A-Za-z.-]+)?",
        normalized,
    )
    if match is None:
        return
    version = tuple(int(part) for part in match.groups())
    if version <= (0, 1, 5):
        raise ValueError(
            f"{normalized} does not support Workspace Remote Embedding "
            "authorization; use @zvec/zvec-grep@0.1.6-alpha.3 or newer, "
            "or pass --zvec-grep-package ../.. "
            "from the benchmarks/swe-qa-bench directory"
        )


def validate_job_destinations(
    jobs_dir: Path, run_specs: Sequence[tuple[Profile, str]]
) -> None:
    names = [job_name for _, job_name in run_specs]
    if len(set(names)) != len(names):
        raise ValueError("profile job names must be unique")
    collisions = [
        jobs_dir / job_name for job_name in names if (jobs_dir / job_name).exists()
    ]
    if collisions:
        paths = ", ".join(str(path.resolve()) for path in collisions)
        raise ValueError(
            f"job output already exists: {paths}; choose a new --job-name or "
            "omit it to use a timestamped name"
        )


def default_job_name(suite: BenchmarkSuite, profile: Profile, *, run_id: str) -> str:
    return f"{run_id}-{suite.name}-{suite.tier}-{profile}"


def profile_job_name(
    suite: BenchmarkSuite,
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
    if agent == _CLAUDE_CODE_AGENT:
        return CLAUDE_CODE_VERSION
    if agent == _CODEX_AGENT:
        return CODEX_VERSION
    if agent == _OPENCODE_AGENT:
        return OPENCODE_VERSION
    raise ValueError(f"agent does not use a setup cache: {agent}")


def uses_setup_cache(agent: str) -> bool:
    return agent in _CACHEABLE_AGENTS


def setup_cache_volume_name(
    agent: str,
    profile: Profile,
    *,
    zvec_grep_package: str = ZVEC_GREP_PACKAGE,
    zvec_grep_package_sha256: str | None = None,
    embedding_model: str = ZVEC_GREP_EMBEDDING,
) -> str:
    identity = f"{agent}-{_agent_version(agent)}-{profile}-linux-x64"
    if profile == "zvec-grep":
        package_identity = (
            f"local-{zvec_grep_package_sha256[:16]}"
            if zvec_grep_package_sha256 is not None
            else zvec_grep_package
        )
        identity += (
            f"-{package_identity}-{ZVEC_GREP_BINDING_PACKAGE}" f"-{embedding_model}"
        )
    return f"zg-bench-{_cache_slug(identity)}"


def setup_cache_compose_path(agent: str, profile: Profile) -> Path:
    return SETUP_CACHE_DIR / f"{_cache_slug(agent)}-{profile}.compose.json"


def claude_code_install_cache_volume_name(profile: Profile) -> str:
    identity = (
        f"{_CLAUDE_CODE_AGENT}-{CLAUDE_CODE_VERSION}-{profile}-linux-x64-local"
    )
    return f"zg-bench-{_cache_slug(identity)}"


def _ensure_external_docker_volume(name: str, *, purpose: str) -> None:
    inspected = subprocess.run(
        ["docker", "volume", "inspect", name],
        check=False,
        capture_output=True,
        text=True,
    )
    if inspected.returncode == 0:
        return

    created = subprocess.run(
        ["docker", "volume", "create", name],
        check=False,
        capture_output=True,
        text=True,
    )
    if created.returncode == 0:
        return

    inspect_detail = (
        inspected.stderr or inspected.stdout or "unknown inspect error"
    ).strip()
    create_detail = (
        created.stderr or created.stdout or "unknown create error"
    ).strip()
    raise RuntimeError(
        f"could not prepare {purpose} Docker volume {name!r}: "
        f"inspect failed ({inspect_detail}); create failed ({create_detail})"
    )


def _cache_local_package(package: Path) -> tuple[Path, str]:
    with package.open("rb") as package_file:
        digest = hashlib.file_digest(package_file, "sha256").hexdigest()
    cached_package = LOCAL_PACKAGE_DIR / f"zvec-grep-{digest[:16]}.tgz"
    if not cached_package.exists():
        shutil.copy2(package, cached_package)
    return cached_package.resolve(), digest


def prepare_local_zvec_grep_package(source_root: Path) -> tuple[Path, str]:
    """Pack a local zvec-grep checkout for installation in task containers."""
    source_root = source_root.expanduser().resolve()
    if not (source_root / "package.json").is_file():
        raise RuntimeError(
            f"local zvec-grep package has no package.json: {source_root}"
        )
    if shutil.which("npm") is None:
        raise RuntimeError("npm is required to pack the local zvec-grep checkout")
    if not (source_root / "node_modules" / ".bin" / "tsc").is_file():
        raise RuntimeError(
            "local zvec-grep dependencies are missing; run 'npm ci' from "
            f"{source_root}"
        )
    LOCAL_PACKAGE_DIR.mkdir(parents=True, exist_ok=True)
    LOCAL_NPM_CACHE_DIR.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(
        prefix="npm-pack-", dir=LOCAL_PACKAGE_DIR
    ) as temp_dir:
        completed = subprocess.run(
            ["npm", "pack", "--pack-destination", temp_dir],
            cwd=source_root,
            check=False,
            capture_output=True,
            text=True,
            env={**os.environ, "npm_config_cache": str(LOCAL_NPM_CACHE_DIR)},
        )
        if completed.returncode != 0:
            detail = (completed.stderr or completed.stdout or "unknown error").strip()
            raise RuntimeError(f"could not pack the local zvec-grep checkout: {detail}")

        packages = list(Path(temp_dir).glob("*.tgz"))
        if len(packages) != 1:
            raise RuntimeError(
                "npm pack did not produce exactly one zvec-grep package: "
                f"found {len(packages)}"
            )
        return _cache_local_package(packages[0])


def _looks_like_package_path(value: str) -> bool:
    return value.startswith((".", "/", "~")) or value.endswith(".tgz")


def normalize_zvec_grep_package(value: str) -> str:
    value = value.strip()
    if not value:
        raise ValueError("zvec-grep package must not be empty")
    if re.fullmatch(r"v?\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?", value):
        return f"@zvec/zvec-grep@{value.removeprefix('v')}"
    return value


def zvec_grep_package_install_spec(value: str) -> str:
    """Return the package spec visible inside a task container."""
    candidate = Path(value).expanduser()
    if candidate.exists() or _looks_like_package_path(value):
        return LOCAL_ZVEC_GREP_PACKAGE_TARGET
    return normalize_zvec_grep_package(value)


def prepare_zvec_grep_package(value: str) -> PreparedZvecGrepPackage:
    normalized = normalize_zvec_grep_package(value)
    candidate = Path(normalized).expanduser()
    if candidate.exists():
        if candidate.is_dir():
            package, digest = prepare_local_zvec_grep_package(candidate)
        elif candidate.is_file() and candidate.suffix == ".tgz":
            LOCAL_PACKAGE_DIR.mkdir(parents=True, exist_ok=True)
            package, digest = _cache_local_package(candidate.resolve())
        else:
            raise ValueError(
                "local zvec-grep package must be a directory or .tgz file: "
                f"{candidate}"
            )
        return PreparedZvecGrepPackage(
            install_spec=LOCAL_ZVEC_GREP_PACKAGE_TARGET,
            bind_source=package,
            sha256=digest,
        )
    if _looks_like_package_path(normalized):
        raise ValueError(f"local zvec-grep package does not exist: {candidate}")
    return PreparedZvecGrepPackage(install_spec=normalized)


def prepare_setup_cache(
    agent: str,
    profile: Profile,
    *,
    zvec_grep_package: str = ZVEC_GREP_PACKAGE,
    embedding_model: str = ZVEC_GREP_EMBEDDING,
) -> PreparedSetupCache:
    """Create the profile-isolated Docker volume and its Compose overlay."""
    prepared_package = PreparedZvecGrepPackage(install_spec=zvec_grep_package)
    if profile == "zvec-grep":
        prepared_package = prepare_zvec_grep_package(zvec_grep_package)

    volume_name = setup_cache_volume_name(
        agent,
        profile,
        zvec_grep_package=prepared_package.install_spec,
        zvec_grep_package_sha256=prepared_package.sha256,
        embedding_model=embedding_model,
    )
    SETUP_CACHE_DIR.mkdir(parents=True, exist_ok=True)
    service_volumes: list[dict[str, Any]] = [
        {
            "type": "volume",
            "source": "agent-setup-cache",
            "target": _SETUP_CACHE_TARGET,
        }
    ]
    compose_volumes: dict[str, dict[str, Any]] = {
        "agent-setup-cache": {
            "external": True,
            "name": volume_name,
        }
    }
    external_volumes = [(volume_name, "agent setup cache")]
    if agent == _CLAUDE_CODE_AGENT:
        claude_install_volume = claude_code_install_cache_volume_name(profile)
        service_volumes.append(
            {
                "type": "volume",
                "source": _CLAUDE_CODE_INSTALL_CACHE_SOURCE,
                "target": _CLAUDE_CODE_INSTALL_CACHE_TARGET,
            }
        )
        compose_volumes[_CLAUDE_CODE_INSTALL_CACHE_SOURCE] = {
            "external": True,
            "name": claude_install_volume,
        }
        external_volumes.append((claude_install_volume, "Claude Code install cache"))
    if profile == "zvec-grep" and embedding_model.startswith("local/"):
        index_seed_path = resolve_zvec_grep_index_seed_dir(
            os.environ.get(ZVEC_GREP_INDEX_SEED_ENV)
        )
    else:
        index_seed_path = None
    if index_seed_path is not None:
        index_seed_path.mkdir(parents=True, exist_ok=True)
        if not index_seed_path.is_dir():
            raise RuntimeError(
                f"zvec-grep index seed path is not a directory: {index_seed_path}"
            )
    if prepared_package.bind_source is not None:
        service_volumes.append(
            {
                "type": "bind",
                "source": str(prepared_package.bind_source),
                "target": LOCAL_ZVEC_GREP_PACKAGE_TARGET,
                "read_only": True,
            }
        )
    overlay = {
        "services": {
            "main": {
                "platform": "linux/amd64",
                # Forward stop signals to Harbor's keepalive and reap children.
                "init": True,
                "volumes": service_volumes,
            }
        },
        "volumes": compose_volumes,
    }
    compose_path = setup_cache_compose_path(agent, profile)
    compose_path.write_text(
        json.dumps(overlay, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )

    for external_volume, purpose in external_volumes:
        _ensure_external_docker_volume(external_volume, purpose=purpose)

    return PreparedSetupCache(
        compose_path=compose_path,
        zvec_grep_package=(
            prepared_package.install_spec if profile == "zvec-grep" else None
        ),
        zvec_grep_package_sha256=prepared_package.sha256,
    )


def build_harbor_command(
    suite: BenchmarkSuite,
    *,
    profile: Profile,
    agent: str,
    model: str,
    jobs_dir: Path = DEFAULT_RUNS_DIR,
    job_name: str,
    n_attempts: int = 1,
    max_retries: int = 0,
    harbor_executable: str = "harbor",
    zvec_grep_package: str = ZVEC_GREP_PACKAGE,
    zvec_grep_package_sha256: str | None = None,
    embedding_model: str = ZVEC_GREP_EMBEDDING,
    embedding_endpoint: str | None = ZVEC_GREP_EMBEDDING_ENDPOINT,
) -> list[str]:
    if profile not in PROFILES:
        raise ValueError(f"unsupported profile: {profile}")
    if (
        isinstance(n_attempts, bool)
        or not isinstance(n_attempts, int)
        or n_attempts < 1
    ):
        raise ValueError("n_attempts must be a positive integer")
    if (
        isinstance(max_retries, bool)
        or not isinstance(max_retries, int)
        or max_retries < 0
    ):
        raise ValueError("max_retries must be a non-negative integer")
    support = resolve_agent_model(agent, model)

    harbor_agent = agent
    harbor_model = model
    agent_kwargs: list[str] = []

    if agent == _CODEX_AGENT:
        agent_kwargs.append(f"version={CODEX_VERSION}")
    elif agent == _CLAUDE_CODE_AGENT:
        agent_kwargs.extend(
            [
                f"version={CLAUDE_CODE_VERSION}",
                f"reasoning_effort={CLAUDE_CODE_REASONING_EFFORT}",
                f"max_budget_usd={CLAUDE_CODE_MAX_BUDGET_USD}",
            ]
        )
    elif agent == _OPENCODE_AGENT:
        harbor_agent = OPENCODE_IMPORT_PATH
        agent_kwargs.append(f"version={OPENCODE_VERSION}")
        agent_kwargs.append("collect_session_usage=true")
        if support.opencode is not None:
            provider = resolve_opencode_model(model).configuration
            harbor_model = provider.harbor_model
            opencode_config = build_opencode_config(provider, profile=profile)
            agent_kwargs.append(
                "opencode_config=" + json.dumps(opencode_config, separators=(",", ":"))
            )

    if profile == "zvec-grep":
        if agent not in _ZVEC_AGENT_IMPORT_PATHS:
            supported = ", ".join(_ZVEC_AGENT_IMPORT_PATHS)
            raise ValueError(
                "the zvec-grep profile currently supports --agent " f"{supported}"
            )
        harbor_agent = _ZVEC_AGENT_IMPORT_PATHS[agent]
        agent_kwargs.extend(
            [
                f"zvec_grep_package={zvec_grep_package}",
                f"zvec_binding_package={ZVEC_GREP_BINDING_PACKAGE}",
                f"embedding_model={embedding_model}",
            ]
        )
        if suite.index_ignore_file is not None:
            agent_kwargs.append(f"index_ignore_file={suite.index_ignore_file}")
        if (
            embedding_endpoint is not None
            and not embedding_model.startswith("local/")
        ):
            agent_kwargs.append(f"embedding_endpoint={embedding_endpoint}")
        if zvec_grep_package_sha256 is not None:
            agent_kwargs.append(f"zvec_grep_package_sha256={zvec_grep_package_sha256}")
        agent_kwargs.append(f"mcp_target={agent}")

    source_args = (
        ["--dataset", suite.dataset]
        if suite.dataset is not None
        else ["--path", str(suite.path)]
    )
    command = [
        harbor_executable,
        "run",
        *source_args,
        "--agent",
        harbor_agent,
        "--model",
        harbor_model,
        "--env",
        "docker",
        "--n-attempts",
        str(n_attempts),
        "--max-retries",
        str(max_retries),
        "--n-concurrent",
        "1",
        "--agent-setup-timeout-multiplier",
        AGENT_SETUP_TIMEOUT_MULTIPLIER,
        "--jobs-dir",
        str(jobs_dir.resolve()),
        "--job-name",
        job_name,
    ]

    if max_retries:
        # Harbor excludes timeouts by default. Retain only its usage-limit
        # exclusion so failed executions (including timeouts) are eligible.
        # The native queue replaces a failed attempt in the same trial slot.
        command.extend(
            [
                "--retry-exclude",
                "ApiUsageLimitError",
                "--plugin",
                "zg_bench.retries:FailedTrialArchivePlugin",
            ]
        )

    if suite.tasks is not None:
        for task in suite.tasks:
            command.extend(["--include-task-name", task])

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
    if support.opencode is not None:
        # Harbor does not automatically forward credentials for our custom
        # provider. Normalizing the host environment alone is insufficient.
        command.extend(["--agent-env", "OPENAI_API_KEY=${OPENAI_API_KEY}"])
    if agent == _CLAUDE_CODE_AGENT:
        credential = _first_nonempty_env(_CLAUDE_CODE_CREDENTIAL_ENV_VARS)
        if credential is not None:
            name, _ = credential
            command.extend(["--agent-env", f"{name}=${{{name}}}"])
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
