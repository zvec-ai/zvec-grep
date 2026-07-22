from __future__ import annotations

import shlex
import time
from datetime import UTC, datetime
from typing import Any

from harbor.agents.installed.claude_code import ClaudeCode
from harbor.environments.base import BaseEnvironment
from harbor.models.trial.paths import EnvironmentPaths

from .zvec_grep import ZvecGrepMixin


class ZvecClaudeCode(ZvecGrepMixin, ClaudeCode):
    """Claude Code with zvec-grep wired in through ``zg install --target claude``.

    Other benchmark agents receive zvec-grep as a CLI plus an injected query
    skill (see :class:`ZvecGrepMixin`). Claude Code instead uses ``zg install``,
    which registers the zvec-grep MCP server, pre-approves its tools, and writes
    managed guidance into Claude Code's configuration directory. The agent then
    reaches the index through the ``mcp__zvec_grep__*`` tools rather than the
    ``zg query`` CLI, so no skill is injected for this profile.
    """

    # ClaudeCode.run() launches the CLI with CLAUDE_CONFIG_DIR set to
    # ``<agent_dir>/sessions``. ``zg install`` honours the same variable, so it
    # must target this directory for the MCP server, permissions, and guidance
    # to be visible when the agent actually runs.
    _CLAUDE_CONFIG_DIR = (EnvironmentPaths.agent_dir / "sessions").as_posix()

    # zvec-grep depends on onnxruntime-node, whose postinstall downloads CUDA
    # binaries from GitHub. That host is unreachable here and any failure makes
    # npm roll the whole install back. The Qwen remote embedding path never uses
    # the local runtime, so skip install scripts entirely.
    _extra_npm_install_flags = "--ignore-scripts"

    async def setup(self, environment: BaseEnvironment) -> None:
        # Install Claude Code itself. ZvecGrepMixin.setup is skipped on purpose:
        # it builds the CLI/skill profile, whereas this adapter provisions the
        # MCP integration below.
        await ClaudeCode.setup(self, environment)

        started_at = datetime.now(UTC)
        started = time.monotonic()
        metadata: dict[str, Any] = {
            "status": "running",
            "integration": "zg-install-claude",
            "started_at": started_at.isoformat(),
            "package": self._zvec_grep_package,
            "binding_package": self._zvec_binding_package,
            "embedding_model": self._embedding_model,
            "api_key_source": self._api_key_source,
            "claude_config_dir": self._CLAUDE_CONFIG_DIR,
        }

        try:
            install_started = time.monotonic()
            zvec_grep_version, reused_install = await self._install_zvec_grep(
                environment
            )
            metadata["zvec_grep_version"] = zvec_grep_version
            metadata["install_reused_cache"] = reused_install
            metadata["install_duration_seconds"] = round(
                time.monotonic() - install_started, 3
            )

            workdir = await self._resolve_workdir(environment)
            metadata["workdir"] = workdir
            metadata["git_exclude_updated"] = await self._hide_index_from_git(
                environment, workdir
            )

            config_dir = shlex.quote(self._CLAUDE_CONFIG_DIR)
            embedding = shlex.quote(self._embedding_model)

            # Register the zvec-grep MCP server, pre-approve its tools, write the
            # managed guidance block, and start the local MCP daemon.
            install_result = await self.exec_as_agent(
                environment,
                command=(
                    f"mkdir -p {config_dir} && "
                    f"CLAUDE_CONFIG_DIR={config_dir} "
                    "zg install --target claude --yes"
                ),
                cwd=workdir,
            )
            metadata["zg_install_stdout"] = self._bounded_output(
                install_result.stdout
            )
            metadata["zg_install_stderr"] = self._bounded_output(
                install_result.stderr
            )

            # Authorize remote embedding for this workspace so the index build
            # and MCP queries run non-interactively.
            grant_result = await self.exec_as_agent(
                environment,
                command=(
                    "zg auth grant --capability embedding --scope workspace "
                    f"--embedding {embedding}"
                ),
                cwd=workdir,
            )
            metadata["auth_grant_stdout"] = self._bounded_output(
                grant_result.stdout
            )

            # Build the index through the daemon that `zg install` started.
            index_started = time.monotonic()
            index_result = await self.exec_as_agent(
                environment,
                command=(
                    "zg index --mode server --allow-remote workspace "
                    f"--embedding {embedding}"
                ),
                cwd=workdir,
            )
            metadata["index_duration_seconds"] = round(
                time.monotonic() - index_started, 3
            )
            metadata["index_stdout"] = self._bounded_output(index_result.stdout)
            metadata["index_stderr"] = self._bounded_output(index_result.stderr)

            status_result = await self.exec_as_agent(
                environment,
                command="zg status",
                cwd=workdir,
            )
            metadata["index_status"] = self._bounded_output(status_result.stdout)
            metadata["index_status_stderr"] = self._bounded_output(
                status_result.stderr
            )
            if not self._index_is_ready(metadata["index_status"]):
                raise RuntimeError(
                    "zvec-grep index setup completed but zg status did not "
                    "report the workspace index as ready"
                )
            metadata["status"] = "ready"
        except BaseException as error:
            metadata["status"] = "failed"
            metadata["error_type"] = type(error).__name__
            metadata["error"] = str(error)
            raise
        finally:
            metadata["finished_at"] = datetime.now(UTC).isoformat()
            metadata["total_duration_seconds"] = round(
                time.monotonic() - started, 3
            )
            self._write_setup_metadata(metadata)

    @staticmethod
    def _index_is_ready(status_output: str) -> bool:
        # `zg status` emits agent markdown (JSON output was removed). A ready
        # index reports "Workspace index is ready"; the not-configured state
        # reports "is not configured", so the substring match is unambiguous.
        return "index is ready" in status_output.lower()
