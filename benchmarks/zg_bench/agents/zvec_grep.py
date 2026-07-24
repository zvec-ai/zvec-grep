from __future__ import annotations

import json
import os
import posixpath
import re
import shlex
import tempfile
import time
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from harbor.agents.installed.node_install import nvm_node_install_snippet
from harbor.environments.base import BaseEnvironment

from ..settings import (
    ZVEC_GREP_API_KEY_ENV_VARS,
    ZVEC_GREP_BINDING_PACKAGE,
    ZVEC_GREP_EMBEDDING,
    ZVEC_GREP_PACKAGE,
)

_SETUP_METADATA_FILENAME = "zvec-grep-setup.json"
_MAX_METADATA_OUTPUT_CHARS = 20_000
_NVM_INIT = 'if [ -s "$HOME/.nvm/nvm.sh" ]; then . "$HOME/.nvm/nvm.sh"; fi;'
_NVM_NODEJS_ORG_MIRROR = "https://npmmirror.com/mirrors/node"
_NPM_CONFIG_REGISTRY = "https://registry.npmmirror.com"


class ZvecGrepMixin:
    """Provision zvec-grep before an installed Harbor agent starts."""

    def __init__(
        self,
        *args: Any,
        zvec_grep_package: str = ZVEC_GREP_PACKAGE,
        zvec_binding_package: str = ZVEC_GREP_BINDING_PACKAGE,
        embedding_model: str = ZVEC_GREP_EMBEDDING,
        mcp_target: str | None = None,
        zvec_grep_package_sha256: str | None = None,
        extra_env: dict[str, str] | None = None,
        **kwargs: Any,
    ) -> None:
        if not zvec_grep_package.strip():
            raise ValueError("zvec_grep_package must not be empty")
        if not zvec_binding_package.strip():
            raise ValueError("zvec_binding_package must not be empty")
        if not embedding_model.strip():
            raise ValueError("embedding_model must not be empty")
        if mcp_target is not None and not mcp_target.strip():
            raise ValueError("mcp_target must not be empty")
        if zvec_grep_package_sha256 is not None and not re.fullmatch(
            r"[0-9a-f]{64}", zvec_grep_package_sha256
        ):
            raise ValueError("zvec_grep_package_sha256 must be a SHA-256 digest")

        resolved_extra_env = dict(extra_env or {})
        api_key_source = self._resolve_api_key_source(resolved_extra_env)
        if embedding_model.startswith("qwen/") and api_key_source is None:
            accepted = ", ".join(ZVEC_GREP_API_KEY_ENV_VARS)
            raise ValueError(
                "Qwen embeddings require an API key; "
                f"export one of: {accepted}"
            )
        api_key = None
        if api_key_source is not None:
            api_key = (
                resolved_extra_env.get(api_key_source)
                or os.environ[api_key_source]
            ).strip()

        # Harbor forwards extra_env through Docker exec flags. Keep embedding
        # credentials out of that path and upload them as a container-only file
        # during setup instead.
        for name in ZVEC_GREP_API_KEY_ENV_VARS:
            resolved_extra_env.pop(name, None)

        super().__init__(*args, extra_env=resolved_extra_env, **kwargs)
        self._zvec_grep_package = zvec_grep_package
        self._zvec_binding_package = zvec_binding_package
        self._embedding_model = embedding_model
        self._mcp_target = mcp_target
        self._zvec_grep_package_sha256 = zvec_grep_package_sha256
        self._api_key_source = api_key_source
        self._embedding_api_key = api_key

    async def setup(self, environment: BaseEnvironment) -> None:
        await super().setup(environment)

        started_at = datetime.now(UTC)
        started = time.monotonic()
        metadata: dict[str, Any] = {
            "status": "running",
            "started_at": started_at.isoformat(),
            "package": self._zvec_grep_package,
            "binding_package": self._zvec_binding_package,
            "embedding_model": self._embedding_model,
            "api_key_source": self._api_key_source,
        }
        if self._zvec_grep_package_sha256 is not None:
            metadata["package_sha256"] = self._zvec_grep_package_sha256
        if self._mcp_target is not None:
            metadata["mcp_target"] = self._mcp_target

        try:
            install_started = time.monotonic()
            zvec_grep_version, reused_install = await self._install_zvec_grep(
                environment
            )
            metadata["zvec_grep_version"] = zvec_grep_version
            metadata["install_reused_cache"] = reused_install
            supports_ready_check = self._supports_machine_ready_check(
                zvec_grep_version,
                package_sha256=self._zvec_grep_package_sha256,
            )
            metadata["ready_check"] = (
                "machine" if supports_ready_check else "legacy-text"
            )
            metadata["install_duration_seconds"] = round(
                time.monotonic() - install_started, 3
            )

            workdir = await self._resolve_workdir(environment)
            metadata["workdir"] = workdir
            metadata["git_exclude_updated"] = await self._hide_index_from_git(
                environment, workdir
            )

            authorization_command = self._authorization_command(
                self._embedding_model
            )
            if authorization_command is not None:
                authorization_started = time.monotonic()
                authorization_result = await self.exec_as_agent(
                    environment,
                    command=authorization_command,
                    cwd=workdir,
                )
                metadata["authorization_duration_seconds"] = round(
                    time.monotonic() - authorization_started, 3
                )
                metadata["authorization_scope"] = "workspace"
                metadata["authorization_stdout"] = self._bounded_output(
                    authorization_result.stdout
                )
                metadata["authorization_stderr"] = self._bounded_output(
                    authorization_result.stderr
                )

            index_started = time.monotonic()
            index_result = await self.exec_as_agent(
                environment,
                command=self._index_command(self._embedding_model),
                cwd=workdir,
            )
            metadata["index_duration_seconds"] = round(
                time.monotonic() - index_started, 3
            )
            metadata["index_stdout"] = self._bounded_output(index_result.stdout)
            metadata["index_stderr"] = self._bounded_output(index_result.stderr)

            status_result = await self.exec_as_agent(
                environment,
                command=(
                    "zg status --check-ready"
                    if supports_ready_check
                    else "zg status"
                ),
                cwd=workdir,
            )
            metadata["index_status"] = self._bounded_output(status_result.stdout)
            metadata["index_status_stderr"] = self._bounded_output(
                status_result.stderr
            )
            if not supports_ready_check and not self._index_is_ready(
                metadata["index_status"]
            ):
                raise RuntimeError(
                    "zvec-grep index setup completed but zg status did not "
                    "report state ready"
                )

            if self._mcp_target is not None:
                await self._setup_mcp(
                    environment,
                    workdir,
                    metadata,
                    supports_ready_check=supports_ready_check,
                )
            metadata["status"] = "ready"
        except BaseException as error:
            metadata["status"] = "failed"
            metadata["error_type"] = type(error).__name__
            metadata["error"] = str(error)
            raise
        finally:
            metadata["finished_at"] = datetime.now(UTC).isoformat()
            metadata["total_duration_seconds"] = round(time.monotonic() - started, 3)
            self._write_setup_metadata(metadata)

    async def _setup_mcp(
        self,
        environment: BaseEnvironment,
        workdir: str,
        metadata: dict[str, Any],
        *,
        supports_ready_check: bool,
    ) -> None:
        assert self._mcp_target is not None
        mcp_started = time.monotonic()
        mcp_result = await self.exec_as_agent(
            environment,
            command=(
                "zg install --target " f"{shlex.quote(self._mcp_target)} --yes"
            ),
            cwd=workdir,
        )
        metadata["mcp_setup_duration_seconds"] = round(
            time.monotonic() - mcp_started, 3
        )
        metadata["mcp_setup_stdout"] = self._bounded_output(mcp_result.stdout)
        metadata["mcp_setup_stderr"] = self._bounded_output(mcp_result.stderr)

        server_status = await self.exec_as_agent(
            environment,
            command=(
                "zg server status --check-ready"
                if supports_ready_check
                else "zg server status"
            ),
            cwd=workdir,
        )
        metadata["mcp_server_status"] = self._bounded_output(server_status.stdout)
        metadata["mcp_server_status_stderr"] = self._bounded_output(
            server_status.stderr
        )
        if not supports_ready_check and not self._mcp_server_is_ready(
            metadata["mcp_server_status"]
        ):
            raise RuntimeError(
                "zvec-grep MCP setup completed but the server did not report ready"
            )

    async def _install_zvec_grep(
        self, environment: BaseEnvironment
    ) -> tuple[str, bool]:
        package = shlex.quote(self._zvec_grep_package)
        binding_package = shlex.quote(self._zvec_binding_package)
        expected_version = self._package_version(self._zvec_grep_package)
        node_install_command = nvm_node_install_snippet()
        ensure_node_command = (
            f"{_NVM_INIT} "
            "if ! command -v node >/dev/null 2>&1 || "
            "! command -v npm >/dev/null 2>&1; then "
            f"{node_install_command}; "
            "fi"
        )
        if self._zvec_grep_package_sha256 is not None:
            expected_digest = shlex.quote(self._zvec_grep_package_sha256)
            install_command = (
                'package_marker="$HOME/.nvm/.zg-bench-zvec-grep-sha256"; '
                "if command -v zg >/dev/null 2>&1 && "
                f'[ "$(cat "$package_marker" 2>/dev/null)" = {expected_digest} ] && '
                f"npm list -g --depth=0 {binding_package} >/dev/null 2>&1; "
                "then reused=1; else "
                f"npm install -g {package} {binding_package} "
                "--omit=optional --no-audit --no-fund && "
                f"printf '%s\\n' {expected_digest} > \"$package_marker\" && "
                "reused=0; fi"
            )
        elif expected_version is None:
            install_command = (
                f"npm install -g {package} {binding_package} "
                "--omit=optional --no-audit --no-fund && reused=0"
            )
        else:
            expected = shlex.quote(expected_version)
            install_command = (
                "if command -v zg >/dev/null 2>&1 && "
                f"[ \"$(zg version -v)\" = {expected} ] && "
                f"npm list -g --depth=0 {binding_package} >/dev/null 2>&1; "
                "then reused=1; else "
                f"npm install -g {package} {binding_package} "
                "--omit=optional --no-audit --no-fund && reused=0; fi"
            )
        install_result = await self.exec_as_agent(
            environment,
            command=(
                "set -e; "
                "export NVM_NODEJS_ORG_MIRROR="
                f"{shlex.quote(_NVM_NODEJS_ORG_MIRROR)}; "
                "export NPM_CONFIG_REGISTRY="
                f"{shlex.quote(_NPM_CONFIG_REGISTRY)}; "
                f"{ensure_node_command}; "
                f"{install_command}; "
                "printf '\\nZG_INSTALL_REUSED=%s\\nZG_NODE_PATH=%s\\n"
                "ZG_BIN_PATH=%s\\n' \"$reused\" \"$(command -v node)\" "
                '"$(command -v zg)"'
            ),
        )
        reused = self._parse_install_value(
            install_result.stdout, marker="ZG_INSTALL_REUSED"
        )
        node_path = self._parse_install_path(
            install_result.stdout, marker="ZG_NODE_PATH"
        )
        zg_path = self._parse_install_path(
            install_result.stdout, marker="ZG_BIN_PATH"
        )

        await self._upload_embedding_config(environment)

        link_commands = [
            f"ln -sf {shlex.quote(zg_path)} /usr/local/bin/zg",
        ]
        if node_path != "/usr/local/bin/node":
            link_commands.append(
                f"ln -sf {shlex.quote(node_path)} /usr/local/bin/node"
            )
        await self.exec_as_root(environment, command=" && ".join(link_commands))
        version_result = await self.exec_as_agent(
            environment, command="zg version -v"
        )
        version = (version_result.stdout or "").strip()
        if not version:
            raise RuntimeError("zg version -v returned no version")
        return version, reused == "1"

    async def _upload_embedding_config(self, environment: BaseEnvironment) -> None:
        if self._embedding_api_key is None:
            return

        home_result = await self.exec_as_agent(
            environment,
            command='printf "%s\\n" "$HOME"',
        )
        home_lines = [
            re.sub(r"\x1b\[[0-9;]*m", "", line).strip()
            for line in (home_result.stdout or "").splitlines()
            if re.sub(r"\x1b\[[0-9;]*m", "", line).strip().startswith("/")
        ]
        home = posixpath.normpath(home_lines[-1] if home_lines else "")
        if not home.startswith("/") or home == "/":
            raise RuntimeError(f"agent home directory is not absolute: {home!r}")

        provider = self._embedding_model.partition("/")[0]
        if not provider:
            raise RuntimeError(
                f"embedding model has no provider: {self._embedding_model!r}"
            )
        remote_config_dir = posixpath.join(home, ".zvec-grep")
        remote_config_path = posixpath.join(remote_config_dir, "config.json")
        await self.exec_as_agent(
            environment,
            command=(
                f"mkdir -p {shlex.quote(remote_config_dir)} && "
                f"chmod 700 {shlex.quote(remote_config_dir)}"
            ),
        )

        with tempfile.TemporaryDirectory(prefix="zg-bench-secret-") as temp_dir:
            config_path = Path(temp_dir) / "config.json"
            config_path.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "providers": {
                            provider: {"apiKey": self._embedding_api_key},
                        },
                    },
                    indent=2,
                    sort_keys=True,
                )
                + "\n",
                encoding="utf-8",
            )
            config_path.chmod(0o600)
            await environment.upload_file(config_path, remote_config_path)

        permission_command = f"chmod 600 {shlex.quote(remote_config_path)}"
        if environment.default_user is not None:
            user = shlex.quote(str(environment.default_user))
            permission_command += (
                f" && chown {user} {shlex.quote(remote_config_path)}"
            )
        await self.exec_as_root(environment, command=permission_command)

        # The container now owns the only remaining copy held by this adapter.
        self._embedding_api_key = None

    @staticmethod
    def _parse_install_path(output: str | None, *, marker: str) -> str:
        value = ZvecGrepMixin._parse_install_value(output, marker=marker)
        normalized = posixpath.normpath(value)
        if not value.startswith("/") or normalized == "/":
            raise RuntimeError(
                f"zvec-grep installed but {marker} did not contain an absolute path"
            )
        return normalized

    @staticmethod
    def _parse_install_value(output: str | None, *, marker: str) -> str:
        prefix = f"{marker}="
        return next(
            (
                line.removeprefix(prefix).strip()
                for line in (output or "").splitlines()
                if line.startswith(prefix)
            ),
            "",
        )

    @staticmethod
    def _package_version(package: str) -> str | None:
        name, separator, version = package.rpartition("@")
        if not separator or not name or not version:
            return None
        return version

    @staticmethod
    def _supports_machine_ready_check(
        version: str, *, package_sha256: str | None
    ) -> bool:
        if package_sha256 is not None:
            return True
        match = re.fullmatch(r"v?(\d+)\.(\d+)\.(\d+)(?:[-+][0-9A-Za-z.-]+)?", version)
        if match is None:
            return False
        return tuple(int(part) for part in match.groups()) >= (0, 1, 6)

    @staticmethod
    def _index_command(embedding_model: str) -> str:
        return f"zg index --embedding {shlex.quote(embedding_model)}"

    @staticmethod
    def _authorization_command(embedding_model: str) -> str | None:
        if embedding_model.startswith("local/"):
            return None
        return (
            "zg auth grant --capability embedding --scope workspace "
            f"--embedding {shlex.quote(embedding_model)}"
        )

    async def _resolve_workdir(self, environment: BaseEnvironment) -> str:
        result = await environment.exec(command="pwd")
        if result.return_code != 0:
            raise RuntimeError("could not resolve the task working directory")

        workdir_lines = [
            re.sub(r"\x1b\[[0-9;]*m", "", line).strip()
            for line in (result.stdout or "").splitlines()
            if re.sub(r"\x1b\[[0-9;]*m", "", line).strip().startswith("/")
        ]
        workdir = posixpath.normpath(workdir_lines[-1] if workdir_lines else "")
        if not workdir or not workdir.startswith("/"):
            raise RuntimeError(f"task working directory is not absolute: {workdir!r}")
        if workdir == "/":
            raise RuntimeError("refusing to index the container root directory")

        check = await environment.exec(
            command=f"test -d {shlex.quote(workdir)}",
        )
        if check.return_code != 0:
            raise RuntimeError(f"task working directory does not exist: {workdir}")
        return workdir

    async def _hide_index_from_git(
        self, environment: BaseEnvironment, workdir: str
    ) -> bool:
        git_check = await environment.exec(
            command="git rev-parse --is-inside-work-tree",
            cwd=workdir,
        )
        if git_check.return_code != 0:
            return False

        await self.exec_as_agent(
            environment,
            command=(
                'exclude_file="$(git rev-parse --git-path info/exclude)"; '
                'mkdir -p "$(dirname "$exclude_file")"; '
                'touch "$exclude_file"; '
                "grep -qxF '.zvec-grep/' \"$exclude_file\" || "
                "printf '\\n.zvec-grep/\\n' >> \"$exclude_file\""
            ),
            cwd=workdir,
        )
        return True

    @staticmethod
    def _resolve_api_key_source(extra_env: dict[str, str]) -> str | None:
        for name in ZVEC_GREP_API_KEY_ENV_VARS:
            value = extra_env.get(name) or os.environ.get(name)
            if value and value.strip():
                return name
        return None

    @staticmethod
    def _index_is_ready(status_output: str) -> bool:
        for line in status_output.splitlines():
            normalized = re.sub(r"\x1b\[[0-9;]*m", "", line).strip()
            if normalized == "✓ Workspace index is ready":
                return True
            fields = normalized.split()
            if len(fields) >= 2 and fields[0] == "state":
                return fields[1] == "ready"
        return False

    @staticmethod
    def _mcp_server_is_ready(status_output: str) -> bool:
        return any(
            line.strip() == "Server: ready" for line in status_output.splitlines()
        )

    @staticmethod
    def _bounded_output(output: str | None) -> str:
        value = (output or "").strip()
        if len(value) <= _MAX_METADATA_OUTPUT_CHARS:
            return value
        omitted = len(value) - _MAX_METADATA_OUTPUT_CHARS
        retained = value[-_MAX_METADATA_OUTPUT_CHARS:]
        return f"[... {omitted} earlier characters omitted ...]\n{retained}"

    def _write_setup_metadata(self, metadata: dict[str, Any]) -> None:
        path = Path(self.logs_dir) / _SETUP_METADATA_FILENAME
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            json.dumps(metadata, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
