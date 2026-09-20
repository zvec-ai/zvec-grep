from __future__ import annotations

import asyncio
import shlex
from typing import override

from harbor.agents.installed.base import NonZeroAgentExitCodeError
from harbor.agents.installed.node_install import DEFAULT_NODE_MAJOR, NVM_VERSION
from harbor.agents.installed.opencode import OpenCode
from harbor.environments.base import BaseEnvironment

_INSTALL_ATTEMPTS = 3


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
    """OpenCode adapter with cache-aware, bounded installation retries."""

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
