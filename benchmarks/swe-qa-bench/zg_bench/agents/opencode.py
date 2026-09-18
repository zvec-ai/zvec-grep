from __future__ import annotations

import asyncio
import json
import shlex
import time
from typing import override

from harbor.agents.installed.base import NonZeroAgentExitCodeError
from harbor.agents.installed.node_install import DEFAULT_NODE_MAJOR, NVM_VERSION
from harbor.agents.installed.opencode import OpenCode
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from zg_bench.agents.opencode_usage import USAGE_SQL, summarize_session_usage

_INSTALL_ATTEMPTS = 3
_USAGE_CAPTURE_TIMEOUT_SECONDS = 30
_USAGE_SNAPSHOT_FILENAME = "opencode-session-snapshot.json"


def resilient_nvm_node_install_snippet(
    node_major: int = DEFAULT_NODE_MAJOR,
) -> str:
    """Install Node through nvm without repeatedly trusting a live pipe."""
    install_url = (
        "https://raw.githubusercontent.com/nvm-sh/nvm/"
        f"{NVM_VERSION}/install.sh"
    )
    return (
        'export NVM_DIR="$HOME/.nvm"; '
        'if [ ! -s "$NVM_DIR/nvm.sh" ]; then '
        'nvm_installer="$(mktemp)"; '
        "trap 'rm -f \"$nvm_installer\"' EXIT; "
        "curl --fail --location --silent --show-error "
        "--retry 3 --retry-all-errors --retry-delay 2 "
        "--connect-timeout 10 --max-time 30 --retry-max-time 90 "
        '--output "$nvm_installer" '
        f"{shlex.quote(install_url)}; "
        'env -u NODE_VERSION bash "$nvm_installer"; '
        'rm -f "$nvm_installer"; '
        "trap - EXIT; "
        "fi; "
        '. "$NVM_DIR/nvm.sh"; '
        "command -v nvm >/dev/null 2>&1 || "
        "{ echo 'Error: NVM failed to load' >&2; exit 1; }; "
        f"nvm install {node_major}; "
        "npm -v"
    )


class ResilientOpenCode(OpenCode):
    """OpenCode with bounded installation retries and recursive usage capture."""

    def __init__(self, *args, collect_session_usage: bool = False, **kwargs):
        if not isinstance(collect_session_usage, bool):
            raise ValueError("collect_session_usage must be a boolean")
        super().__init__(*args, **kwargs)
        self._collect_session_usage = collect_session_usage
        self._usage_capture_complete = False
        self._usage_capture_error: str | None = None
        self._usage_collection_wall_seconds = 0.0

    @override
    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        if not self._collect_session_usage:
            await super().run(instruction, environment, context)
            return
        execution_complete = False
        self._usage_capture_complete = False
        self._usage_capture_error = None
        try:
            await super().run(instruction, environment, context)
            execution_complete = True
        finally:
            # Harbor downloads /logs/agent after run() but before stopping the
            # container. Export here: root stdout/export alone omits children.
            # Never fill context here; Harbor skips post-run parsing otherwise.
            started = time.monotonic()
            destination = f"/logs/agent/{_USAGE_SNAPSHOT_FILENAME}"
            command = (
                "set -e; . ~/.nvm/nvm.sh; "
                f"opencode db {shlex.quote(USAGE_SQL)} --format json "
                f"> {shlex.quote(destination + '.tmp')}; "
                f"mv {shlex.quote(destination + '.tmp')} {shlex.quote(destination)}"
            )
            try:
                await asyncio.wait_for(
                    self.exec_as_agent(
                        environment, command=command,
                        timeout_sec=_USAGE_CAPTURE_TIMEOUT_SECONDS,
                    ),
                    timeout=_USAGE_CAPTURE_TIMEOUT_SECONDS,
                )
                # A timeout may leave OpenCode writing in the container; that
                # snapshot is useful evidence but never a complete comparison.
                self._usage_capture_complete = execution_complete
            except Exception as exc:
                # Preserve the original execution error, and fail closed in
                # collection. Only the exception type enters the public report.
                self._usage_capture_error = type(exc).__name__
            finally:
                self._usage_collection_wall_seconds = time.monotonic() - started

    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        super().populate_context_post_run(context)
        if not self._collect_session_usage:
            return
        events = self._parse_stdout()
        root_ids = {event.get("sessionID") for event in events if event.get("sessionID")}
        root_session_id = next(iter(root_ids)) if len(root_ids) == 1 else ""
        capture_complete = self._usage_capture_complete and len(root_ids) == 1
        snapshot_error = None
        try:
            snapshot = json.loads(
                (self.logs_dir / _USAGE_SNAPSHOT_FILENAME).read_text()
            )
        except (OSError, ValueError) as exc:
            snapshot = []
            capture_complete = False
            snapshot_error = type(exc).__name__
        usage = summarize_session_usage(
            snapshot, root_session_id, capture_complete=capture_complete
        )
        if self._usage_capture_error or snapshot_error:
            usage["errors"].append(
                "snapshot_capture_failed:" + (self._usage_capture_error or snapshot_error)
            )
        usage["collection_wall_seconds"] = self._usage_collection_wall_seconds
        self.logs_dir.mkdir(parents=True, exist_ok=True)
        (self.logs_dir / "session-usage.json").write_text(
            json.dumps(usage, indent=2, sort_keys=True) + "\n"
        )
        total = usage["total"]
        context.n_input_tokens = total["input_tokens"]
        context.n_output_tokens = total["output_tokens"]
        context.n_cache_tokens = total["cache_read_tokens"] + total["cache_write_tokens"]
        context.cost_usd = total["cost_usd"]
        context.metadata = {**(context.metadata or {}), "session_usage": usage}

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        await self.exec_as_root(
            environment,
            command="apt-get update && apt-get install -y curl",
            env={"DEBIAN_FRONTEND": "noninteractive"},
        )

        version_spec = f"@{self._version}" if self._version else "@latest"
        package_spec = shlex.quote(f"opencode-ai{version_spec}")
        if self._version:
            expected_version = shlex.quote(str(self._version))
            install_opencode = (
                'installed_opencode_version="$(opencode --version '
                '2>/dev/null || true)"; '
                f'if [ "$installed_opencode_version" != {expected_version} ]; then '
                "npm_config_fetch_retries=4 "
                "npm_config_fetch_retry_mintimeout=2000 "
                "npm_config_fetch_retry_maxtimeout=20000 "
                f"npm install --global {package_spec}; "
                "fi"
            )
        else:
            install_opencode = (
                "npm_config_fetch_retries=4 "
                "npm_config_fetch_retry_mintimeout=2000 "
                "npm_config_fetch_retry_maxtimeout=20000 "
                f"npm install --global {package_spec}"
            )

        command = (
            "set -euo pipefail; "
            f"{resilient_nvm_node_install_snippet()}; "
            f"{install_opencode}; "
            "opencode --version"
        )
        for attempt in range(_INSTALL_ATTEMPTS):
            try:
                await self.exec_as_agent(environment, command=command)
                return
            except NonZeroAgentExitCodeError:
                if attempt == _INSTALL_ATTEMPTS - 1:
                    raise
                await asyncio.sleep(2**attempt)
