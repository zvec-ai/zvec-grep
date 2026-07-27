from __future__ import annotations

import asyncio
import hashlib
import json
import os
import re
import shutil
import subprocess
import tarfile
import tempfile
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Literal, Sequence

import yaml

from .github_proxy import (
    normalize_github_proxy_prefix,
    rewrite_github_downloads,
)
from .settings import (
    AGENT_SETUP_TIMEOUT_MULTIPLIER,
    CODEX_VERSION,
    OPENCODE_ALIYUN_GLM_MODEL,
    OPENCODE_ALIYUN_GLM_MODEL_ID,
    OPENCODE_ALIYUN_QWEN_MODEL,
    OPENCODE_ALIYUN_QWEN_MODEL_ID,
    OPENCODE_DASHSCOPE_BASE_URL,
    OPENCODE_OPENAI_COMPATIBLE_PACKAGE,
    OPENCODE_VERSION,
    PIP_INDEX_URL,
    QWEN_CODE_DASHSCOPE_BASE_URL,
    QWEN_CODE_DASHSCOPE_MODEL,
    QWEN_CODE_VERSION,
    UV_DEFAULT_INDEX,
    UV_VERSION,
    ZVEC_GREP_API_KEY_ENV_VARS,
    ZVEC_GREP_BINDING_PACKAGE,
    ZVEC_GREP_EMBEDDING,
    ZVEC_GREP_PACKAGE,
)

BENCHMARKS_DIR = Path(__file__).resolve().parents[1]
SUITES_DIR = BENCHMARKS_DIR / "suites"
DEFAULT_RUNS_DIR = BENCHMARKS_DIR / "runs"
DEFAULT_ZVEC_INDEX_CACHE_DIR = (
    BENCHMARKS_DIR / ".cache" / "zvec-grep-indexes"
)
ZVEC_GREP_SKILL_DIR = BENCHMARKS_DIR / "skills" / "zvec-grep"
ZVEC_CODEX_IMPORT_PATH = "zg_bench.agents.zvec_codex:ZvecCodex"
ZVEC_QWEN_CODE_IMPORT_PATH = (
    "zg_bench.agents.zvec_qwen_code:ZvecQwenCode"
)
ZVEC_OPENCODE_IMPORT_PATH = "zg_bench.agents.zvec_opencode:ZvecOpenCode"
PROXY_CODEX_IMPORT_PATH = "zg_bench.github_proxy:ProxyCodex"
PROXY_OPENCODE_IMPORT_PATH = "zg_bench.github_proxy:ProxyOpenCode"
PROXY_QWEN_CODE_IMPORT_PATH = "zg_bench.github_proxy:ProxyQwenCode"
SETUP_CACHE_DIR = BENCHMARKS_DIR / ".cache" / "agent-setup"
LOCAL_PACKAGE_DIR = SETUP_CACHE_DIR / "local-package"
LOCAL_NPM_CACHE_DIR = SETUP_CACHE_DIR / "npm-cache"
LOCAL_UV_TASKS_DIR = SETUP_CACHE_DIR / "uv-tasks"
LOCAL_ZVEC_GREP_PACKAGE_TARGET = "/tmp/zg-bench-zvec-grep.tgz"
LOCAL_UV_ARCHIVE_NAME = "zg-bench-uv.tar.gz"
ZVEC_INDEX_CACHE_TARGET = "/opt/zvec-grep-index-cache"
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
Tier = Literal["smoke", "ci", "full"]
PROFILES: tuple[Profile, ...] = ("baseline", "zvec-grep")
PROFILE_SELECTIONS: tuple[ProfileSelection, ...] = (*PROFILES, "all")
TIERS: tuple[Tier, ...] = ("smoke", "ci", "full")


class SuiteConfigError(ValueError):
    """Raised when a benchmark suite definition is invalid."""


@dataclass(frozen=True)
class BenchmarkSuite:
    name: str
    dataset: str
    tier: Tier
    tasks: tuple[str, ...] | None


@dataclass(frozen=True)
class PreparedUvTasks:
    dataset_path: Path
    task_count: int


@dataclass(frozen=True)
class AgentModelSupport:
    """An agent/model pair intentionally supported by this benchmark."""

    agent: str
    model: str
    aliases: tuple[str, ...] = ()
    configuration: str = "configured"

    def matches(self, agent: str, model: str) -> bool:
        return self.agent == agent and model in (self.model, *self.aliases)


_CODEX_MODEL_SUPPORT = AgentModelSupport(
    _CODEX_AGENT,
    "*",
    configuration="native passthrough",
)
_QWEN_CODE_MODEL_SUPPORT = AgentModelSupport(
    _QWEN_CODE_AGENT,
    QWEN_CODE_DASHSCOPE_MODEL,
    aliases=(
        f"dashscope/{QWEN_CODE_DASHSCOPE_MODEL}",
        "qwen-3.7-max",
        "dashscope/qwen-3.7-max",
    ),
)
_OPENCODE_GLM_MODEL_SUPPORT = AgentModelSupport(
    _OPENCODE_AGENT,
    OPENCODE_ALIYUN_GLM_MODEL,
    aliases=(
        f"openai/{OPENCODE_ALIYUN_GLM_MODEL}",
        f"dashscope/{OPENCODE_ALIYUN_GLM_MODEL}",
    ),
)
_OPENCODE_QWEN_MODEL_SUPPORT = AgentModelSupport(
    _OPENCODE_AGENT,
    OPENCODE_ALIYUN_QWEN_MODEL,
    aliases=(f"dashscope/{OPENCODE_ALIYUN_QWEN_MODEL}",),
)
AGENT_MODEL_SUPPORT: tuple[AgentModelSupport, ...] = (
    # Codex owns its model catalog and receives the selected model unchanged.
    _CODEX_MODEL_SUPPORT,
    _QWEN_CODE_MODEL_SUPPORT,
    _OPENCODE_GLM_MODEL_SUPPORT,
    _OPENCODE_QWEN_MODEL_SUPPORT,
)


@dataclass(frozen=True)
class PreparedSetupCache:
    compose_path: Path
    zvec_grep_package: str | None = None
    zvec_grep_package_sha256: str | None = None
    zvec_index_cache_dir: Path | None = None
    zvec_index_cache_error: str | None = None


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
    dataset = _require_nonempty_string(root.get("dataset"), "dataset")
    if "@" not in dataset or dataset.endswith("@latest"):
        raise SuiteConfigError("dataset must use a pinned Harbor revision")

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
        raise SuiteConfigError(
            f"tiers.{tier} cannot define both all: true and tasks"
        )
    if run_all:
        tasks: tuple[str, ...] | None = None
    else:
        if not isinstance(raw_tasks, list) or not raw_tasks:
            raise SuiteConfigError(
                f"tiers.{tier} must contain tasks or set all: true"
            )
        tasks = tuple(
            _require_nonempty_string(task, f"tiers.{tier}.tasks[{index}]")
            for index, task in enumerate(raw_tasks)
        )
        if len(set(tasks)) != len(tasks):
            raise SuiteConfigError(f"tiers.{tier}.tasks must not contain duplicates")
    if tier == "smoke" and tasks is not None and len(tasks) != 1:
        raise SuiteConfigError("the smoke tier must contain exactly one task")

    if task_overrides:
        tasks = tuple(
            _require_nonempty_string(task, f"task override {index}")
            for index, task in enumerate(task_overrides)
        )
        if len(set(tasks)) != len(tasks):
            raise SuiteConfigError("task overrides must not contain duplicates")

    if path.parent == SUITES_DIR and name != path.stem:
        raise SuiteConfigError(f"suite name {name!r} must match filename {path.stem!r}")

    return BenchmarkSuite(name=name, dataset=dataset, tier=tier, tasks=tasks)


def new_run_id() -> str:
    return datetime.now(UTC).strftime("%Y%m%d-%H%M%S")


def selected_profiles(selection: ProfileSelection) -> tuple[Profile, ...]:
    if selection == "all":
        return PROFILES
    if selection not in PROFILES:
        raise ValueError(f"unsupported profile: {selection}")
    return (selection,)


def available_agent_models() -> tuple[AgentModelSupport, ...]:
    return AGENT_MODEL_SUPPORT


def resolve_agent_model(agent: str, model: str) -> AgentModelSupport:
    agent = agent.strip()
    model = model.strip()
    if not agent:
        raise ValueError("agent must not be empty")
    if not model:
        raise ValueError("model must not be empty")
    agent_support = tuple(
        support for support in AGENT_MODEL_SUPPORT if support.agent == agent
    )
    if not agent_support:
        supported = ", ".join(
            dict.fromkeys(support.agent for support in AGENT_MODEL_SUPPORT)
        )
        raise ValueError(
            f"unsupported agent {agent!r}; supported agents: {supported}"
        )

    for support in agent_support:
        if support.model == "*" or support.matches(agent, model):
            return support

    supported = ", ".join(support.model for support in agent_support)
    raise ValueError(
        f"unsupported model {model!r} for agent {agent!r}; "
        f"supported models: {supported}"
    )


def _is_qwen_code_dashscope_model(agent: str, model: str) -> bool:
    return _QWEN_CODE_MODEL_SUPPORT.matches(agent, model)


def _is_opencode_aliyun_glm_model(agent: str, model: str) -> bool:
    return _OPENCODE_GLM_MODEL_SUPPORT.matches(agent, model)


def _is_opencode_aliyun_qwen_model(agent: str, model: str) -> bool:
    return _OPENCODE_QWEN_MODEL_SUPPORT.matches(agent, model)


def _opencode_dashscope_model_id(agent: str, model: str) -> str | None:
    if _is_opencode_aliyun_glm_model(agent, model):
        return OPENCODE_ALIYUN_GLM_MODEL_ID
    if _is_opencode_aliyun_qwen_model(agent, model):
        return OPENCODE_ALIYUN_QWEN_MODEL_ID
    return None


def _first_nonempty_env(names: Sequence[str]) -> tuple[str, str] | None:
    for name in names:
        value = os.environ.get(name, "").strip()
        if value:
            return name, value
    return None


def validate_profile_credentials(
    profiles: Sequence[Profile], *, agent: str, model: str
) -> None:
    resolve_agent_model(agent, model)
    if _is_qwen_code_dashscope_model(agent, model):
        if _first_nonempty_env(_QWEN_CODE_API_KEY_ENV_VARS) is None:
            accepted = ", ".join(_QWEN_CODE_API_KEY_ENV_VARS)
            raise ValueError(
                f"{QWEN_CODE_DASHSCOPE_MODEL} requires a DashScope API key; "
                f"export one of: {accepted}"
            )

    if _opencode_dashscope_model_id(agent, model) is not None:
        if _first_nonempty_env(_OPENCODE_ALIYUN_API_KEY_ENV_VARS) is None:
            accepted = ", ".join(_OPENCODE_ALIYUN_API_KEY_ENV_VARS)
            raise ValueError(
                f"{model} requires a DashScope API key; "
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
            "local zvec-grep package must be a directory or .tgz file: "
            f"{candidate}"
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
            "or pass --zvec-grep-package .. "
            "from the benchmarks directory"
        )


def validate_job_destinations(
    jobs_dir: Path, run_specs: Sequence[tuple[Profile, str]]
) -> None:
    names = [job_name for _, job_name in run_specs]
    if len(set(names)) != len(names):
        raise ValueError("profile job names must be unique")
    collisions = [jobs_dir / job_name for job_name in names if (jobs_dir / job_name).exists()]
    if collisions:
        paths = ", ".join(str(path.resolve()) for path in collisions)
        raise ValueError(
            f"job output already exists: {paths}; choose a new --job-name or "
            "omit it to use a timestamped name"
        )


def execution_environment(*, agent: str, model: str) -> dict[str, str]:
    """Return Harbor's environment without placing credentials in its command."""
    resolve_agent_model(agent, model)
    environment = os.environ.copy()
    if _is_qwen_code_dashscope_model(agent, model):
        credential = _first_nonempty_env(_QWEN_CODE_API_KEY_ENV_VARS)
        if credential is not None:
            _, api_key = credential
            environment["OPENAI_API_KEY"] = api_key
    if _opencode_dashscope_model_id(agent, model) is not None:
        credential = _first_nonempty_env(_OPENCODE_ALIYUN_API_KEY_ENV_VARS)
        if credential is not None:
            _, api_key = credential
            environment["OPENAI_API_KEY"] = api_key
        environment["OPENAI_BASE_URL"] = OPENCODE_DASHSCOPE_BASE_URL
    return environment


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
    if agent == _CODEX_AGENT:
        return CODEX_VERSION
    if agent == _QWEN_CODE_AGENT:
        return QWEN_CODE_VERSION
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
) -> str:
    identity = f"{agent}-{_agent_version(agent)}-{profile}-linux-x64"
    if profile == "zvec-grep":
        package_identity = (
            f"local-{zvec_grep_package_sha256[:16]}"
            if zvec_grep_package_sha256 is not None
            else zvec_grep_package
        )
        identity += f"-{package_identity}-{ZVEC_GREP_BINDING_PACKAGE}"
    return f"zg-bench-{_cache_slug(identity)}"


def setup_cache_compose_path(agent: str, profile: Profile) -> Path:
    return SETUP_CACHE_DIR / f"{_cache_slug(agent)}-{profile}.compose.json"


def validate_uv_archive(archive: Path) -> Path:
    """Validate an official-style uv release archive before using it in builds."""
    archive = archive.expanduser().resolve()
    if not archive.is_file():
        raise ValueError(f"uv archive does not exist: {archive}")

    try:
        with tarfile.open(archive, "r:gz") as contents:
            members = contents.getmembers()
    except (tarfile.TarError, OSError) as error:
        raise ValueError(
            f"uv archive is not a readable tar.gz file: {archive}"
        ) from error

    unsafe = [
        member.name
        for member in members
        if Path(member.name).is_absolute() or ".." in Path(member.name).parts
    ]
    if unsafe:
        raise ValueError(f"uv archive contains an unsafe path: {unsafe[0]}")

    binary_members = [
        member
        for member in members
        if member.isfile() and Path(member.name).name in {"uv", "uvx"}
    ]
    binaries = {Path(member.name).name for member in binary_members}
    missing = sorted({"uv", "uvx"} - binaries)
    if missing:
        raise ValueError(
            f"uv archive is missing required binary: {', '.join(missing)}"
        )
    binary_paths = [Path(member.name).parts for member in binary_members]
    if any(len(parts) != 2 for parts in binary_paths) or len(
        {parts[0] for parts in binary_paths}
    ) != 1:
        raise ValueError(
            "uv archive must contain uv and uvx under one top-level directory"
        )
    return archive


def patch_task_uv_install(task_dir: Path, archive: Path) -> bool:
    """Replace Harbor's online uv install layer with a local release archive."""
    archive = validate_uv_archive(archive)
    environment_dir = task_dir / "environment"
    dockerfile = environment_dir / "Dockerfile"
    if not dockerfile.is_file():
        return False

    online_install = (
        f"RUN curl -LsSf https://astral.sh/uv/{UV_VERSION}/install.sh | sh"
    )
    local_install = (
        f"COPY {LOCAL_UV_ARCHIVE_NAME} /tmp/{LOCAL_UV_ARCHIVE_NAME}\n"
        "RUN mkdir -p /tmp/zg-bench-uv \\\n"
        f"    && tar -xzf /tmp/{LOCAL_UV_ARCHIVE_NAME} "
        "-C /tmp/zg-bench-uv --strip-components=1 --no-same-owner \\\n"
        "    && install -m 0755 /tmp/zg-bench-uv/uv /usr/local/bin/uv \\\n"
        "    && install -m 0755 /tmp/zg-bench-uv/uvx /usr/local/bin/uvx \\\n"
        f"    && rm -rf /tmp/zg-bench-uv /tmp/{LOCAL_UV_ARCHIVE_NAME}"
    )
    contents = dockerfile.read_text(encoding="utf-8")
    if online_install in contents:
        dockerfile.write_text(
            contents.replace(online_install, local_install, 1),
            encoding="utf-8",
        )
    elif f"COPY {LOCAL_UV_ARCHIVE_NAME} " not in contents:
        return False

    shutil.copy2(archive, environment_dir / LOCAL_UV_ARCHIVE_NAME)
    return True


def patch_task_github_downloads(task_dir: Path, proxy_prefix: str) -> int:
    """Rewrite task setup files that download content from GitHub."""
    proxy_prefix = normalize_github_proxy_prefix(proxy_prefix)
    candidates = {
        path
        for path in (task_dir / "environment").rglob("*")
        if path.is_file()
    }
    candidates.update(
        path for path in task_dir.rglob("*.sh") if path.is_file()
    )

    patched = 0
    for path in candidates:
        try:
            contents = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        rewritten = rewrite_github_downloads(contents, proxy_prefix)
        if rewritten == contents:
            continue
        path.write_text(rewritten, encoding="utf-8")
        patched += 1
    return patched


async def _download_suite_task_paths(
    suite: BenchmarkSuite,
) -> tuple[tuple[str, Path], ...]:
    """Resolve and cache the same task packages selected by the Harbor command."""
    from harbor.models.job.config import DatasetConfig
    from harbor.tasks.client import TaskClient

    if "@" in suite.dataset:
        dataset_name, dataset_ref = suite.dataset.split("@", 1)
    else:
        dataset_name, dataset_ref = suite.dataset, "latest"
    dataset = DatasetConfig(
        name=dataset_name,
        ref=dataset_ref,
        task_names=list(suite.tasks) if suite.tasks is not None else None,
    )
    task_configs = await dataset.get_task_configs()
    task_ids = [config.get_task_id() for config in task_configs]
    result = await TaskClient().download_tasks(
        task_ids
    )
    return tuple(
        (task_id.get_name(), path)
        for task_id, path in zip(task_ids, result.paths, strict=True)
    )


def prepare_suite_task_overrides(
    suite: BenchmarkSuite,
    *,
    uv_archive: Path | None = None,
    github_proxy_prefix: str | None = None,
) -> PreparedUvTasks:
    """Create isolated task copies with requested download overrides."""
    if uv_archive is None and github_proxy_prefix is None:
        raise ValueError("at least one task download override is required")
    if uv_archive is not None:
        uv_archive = validate_uv_archive(uv_archive)
    if github_proxy_prefix is not None:
        github_proxy_prefix = normalize_github_proxy_prefix(
            github_proxy_prefix
        )

    try:
        downloaded_tasks = asyncio.run(_download_suite_task_paths(suite))
    except Exception as error:
        raise RuntimeError("could not cache the selected Harbor tasks") from error

    archive_digest = None
    if uv_archive is not None:
        with uv_archive.open("rb") as archive_file:
            archive_digest = hashlib.file_digest(
                archive_file, "sha256"
            ).hexdigest()
    cache_identity = json.dumps(
        {
            "dataset": suite.dataset,
            "tasks": suite.tasks,
            "uv_archive_sha256": archive_digest,
            "github_proxy_prefix": github_proxy_prefix,
        },
        sort_keys=True,
    ).encode()
    cache_digest = hashlib.sha256(cache_identity).hexdigest()[:16]
    dataset_path = LOCAL_UV_TASKS_DIR / cache_digest
    dataset_path.mkdir(parents=True, exist_ok=True)

    missed_tasks: list[str] = []
    local_names: set[str] = set()
    for task_name, task_path in downloaded_tasks:
        local_name = task_name.rsplit("/", 1)[-1]
        if local_name in local_names:
            raise RuntimeError(
                f"selected tasks have duplicate local name: {local_name}"
            )
        local_names.add(local_name)
        local_task_path = dataset_path / local_name
        shutil.copytree(task_path, local_task_path, dirs_exist_ok=True)
        if uv_archive is not None and not patch_task_uv_install(
            local_task_path, uv_archive
        ):
            missed_tasks.append(task_name)
        if github_proxy_prefix is not None:
            patch_task_github_downloads(
                local_task_path,
                github_proxy_prefix,
            )

    if missed_tasks:
        names = ", ".join(missed_tasks)
        raise RuntimeError(
            f"could not replace Harbor's uv {UV_VERSION} install layer in: {names}"
        )
    return PreparedUvTasks(
        dataset_path=dataset_path.resolve(),
        task_count=len(downloaded_tasks),
    )


def prepare_suite_uv_archive(
    suite: BenchmarkSuite, archive: Path
) -> PreparedUvTasks:
    """Create isolated task copies that install uv from a local archive."""
    return prepare_suite_task_overrides(suite, uv_archive=archive)


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
        raise RuntimeError(f"local zvec-grep package has no package.json: {source_root}")
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
    zvec_index_cache_dir: Path | None = None,
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
    )
    SETUP_CACHE_DIR.mkdir(parents=True, exist_ok=True)
    service_volumes: list[dict[str, Any]] = [
        {
            "type": "volume",
            "source": "agent-setup-cache",
            "target": _SETUP_CACHE_TARGET,
        }
    ]
    resolved_index_cache_dir: Path | None = None
    index_cache_error: str | None = None
    if prepared_package.bind_source is not None:
        service_volumes.append(
            {
                "type": "bind",
                "source": str(prepared_package.bind_source),
                "target": LOCAL_ZVEC_GREP_PACKAGE_TARGET,
                "read_only": True,
            }
        )
    if profile == "zvec-grep" and zvec_index_cache_dir is not None:
        try:
            resolved_index_cache_dir = (
                zvec_index_cache_dir.expanduser().resolve()
            )
            resolved_index_cache_dir.mkdir(parents=True, exist_ok=True)
        except OSError as error:
            resolved_index_cache_dir = None
            index_cache_error = str(error)
        else:
            service_volumes.append(
                {
                    "type": "bind",
                    "source": str(resolved_index_cache_dir),
                    "target": ZVEC_INDEX_CACHE_TARGET,
                }
            )
    overlay = {
        "services": {
            "main": {
                "environment": {
                    "PIP_INDEX_URL": PIP_INDEX_URL,
                    "UV_DEFAULT_INDEX": UV_DEFAULT_INDEX,
                },
                "platform": "linux/amd64",
                "volumes": service_volumes,
            }
        },
        "volumes": {
            "agent-setup-cache": {
                "external": True,
                "name": volume_name,
            }
        },
    }
    compose_path = setup_cache_compose_path(agent, profile)
    compose_path.write_text(
        json.dumps(overlay, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )

    inspected = subprocess.run(
        ["docker", "volume", "inspect", volume_name],
        check=False,
        capture_output=True,
        text=True,
    )
    if inspected.returncode != 0:
        completed = subprocess.run(
            ["docker", "volume", "create", volume_name],
            check=False,
            capture_output=True,
            text=True,
        )
        if completed.returncode != 0:
            detail = (completed.stderr or completed.stdout or "unknown error").strip()
            raise RuntimeError(f"could not prepare agent setup cache: {detail}")

    return PreparedSetupCache(
        compose_path=compose_path,
        zvec_grep_package=(
            prepared_package.install_spec if profile == "zvec-grep" else None
        ),
        zvec_grep_package_sha256=prepared_package.sha256,
        zvec_index_cache_dir=resolved_index_cache_dir,
        zvec_index_cache_error=index_cache_error,
    )


def build_harbor_command(
    suite: BenchmarkSuite,
    *,
    profile: Profile,
    agent: str,
    model: str,
    jobs_dir: Path = DEFAULT_RUNS_DIR,
    job_name: str,
    harbor_executable: str = "harbor",
    zvec_grep_package: str = ZVEC_GREP_PACKAGE,
    zvec_grep_package_sha256: str | None = None,
    task_dataset_path: Path | None = None,
    github_proxy_prefix: str | None = None,
    zvec_index_cache_dir: Path | None = None,
) -> list[str]:
    if profile not in PROFILES:
        raise ValueError(f"unsupported profile: {profile}")
    resolve_agent_model(agent, model)

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
        opencode_model_id = _opencode_dashscope_model_id(agent, model)
        if opencode_model_id is not None:
            harbor_model = f"dashscope/{opencode_model_id}"
            opencode_config = {
                "provider": {
                    "dashscope": {
                        "npm": OPENCODE_OPENAI_COMPATIBLE_PACKAGE,
                        "name": "DashScope OpenAI Compatible",
                        "models": {
                            opencode_model_id: {
                                "options": {"enable_thinking": False}
                            }
                        },
                        "options": {
                            "apiKey": "{env:OPENAI_API_KEY}",
                            "baseURL": OPENCODE_DASHSCOPE_BASE_URL,
                        },
                    }
                }
            }
            if profile == "zvec-grep":
                # ZvecGrepMixin provisions this entry during setup, but the
                # native OpenCode adapter renders opencode.json again just
                # before execution. Include the managed MCP entry in that
                # render so the provider config does not overwrite it.
                opencode_config["mcp"] = {
                    "zvec_grep": {
                        "type": "remote",
                        "url": "http://127.0.0.1:7999/mcp",
                        "enabled": True,
                        "timeout": 600_000,
                        "oauth": False,
                    }
                }
            agent_kwargs.append(
                "opencode_config="
                + json.dumps(opencode_config, separators=(",", ":"))
            )

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
                f"zvec_grep_package={zvec_grep_package}",
                f"zvec_binding_package={ZVEC_GREP_BINDING_PACKAGE}",
                f"embedding_model={ZVEC_GREP_EMBEDDING}",
            ]
        )
        if zvec_index_cache_dir is not None:
            agent_kwargs.extend(
                [
                    "index_cache_host_root="
                    + str(zvec_index_cache_dir.expanduser().resolve()),
                    f"index_cache_container_root={ZVEC_INDEX_CACHE_TARGET}",
                    "index_cache_dataset=" + suite.dataset.split("@", 1)[0],
                ]
            )
        if zvec_grep_package_sha256 is not None:
            agent_kwargs.append(
                f"zvec_grep_package_sha256={zvec_grep_package_sha256}"
            )
        if agent in {_CODEX_AGENT, _OPENCODE_AGENT}:
            agent_kwargs.append(f"mcp_target={agent}")
        else:
            if not ZVEC_GREP_SKILL_DIR.is_dir():
                raise ValueError(
                    f"zvec-grep skill not found: {ZVEC_GREP_SKILL_DIR}"
                )
            skills.append(str(ZVEC_GREP_SKILL_DIR.resolve()))

    if github_proxy_prefix is not None:
        github_proxy_prefix = normalize_github_proxy_prefix(
            github_proxy_prefix
        )
        if profile == "baseline":
            proxy_agents = {
                _CODEX_AGENT: PROXY_CODEX_IMPORT_PATH,
                _QWEN_CODE_AGENT: PROXY_QWEN_CODE_IMPORT_PATH,
                _OPENCODE_AGENT: PROXY_OPENCODE_IMPORT_PATH,
            }
            harbor_agent = proxy_agents[agent]
        agent_kwargs.append(
            f"github_proxy_prefix={github_proxy_prefix}"
        )

    command = [
        harbor_executable,
        "run",
    ]
    if task_dataset_path is None:
        command.extend(["--dataset", suite.dataset])
    else:
        command.extend(["--path", str(task_dataset_path.resolve())])
    command.extend(
        [
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
    )

    if task_dataset_path is None and suite.tasks is not None:
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
    if _opencode_dashscope_model_id(agent, model) is not None:
        command.extend(
            ["--agent-env", "OPENAI_API_KEY=${OPENAI_API_KEY}"]
        )
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
