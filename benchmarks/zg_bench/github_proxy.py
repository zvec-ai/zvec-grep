from __future__ import annotations

import re
from typing import Any
from urllib.parse import urlsplit

from harbor.agents.installed.codex import Codex
from harbor.agents.installed.opencode import OpenCode
from harbor.agents.installed.qwen_code import QwenCode
from harbor.environments.base import BaseEnvironment

_GITHUB_DOWNLOAD_ORIGINS = (
    "https://github.com/",
    "https://raw.githubusercontent.com/",
    "https://objects.githubusercontent.com/",
    "https://codeload.github.com/",
)
_GITHUB_DOWNLOAD_PATTERN = re.compile(
    "|".join(re.escape(origin) for origin in _GITHUB_DOWNLOAD_ORIGINS)
)
_UV_INSTALLER_PATTERN = re.compile(
    r"https://astral\.sh/uv/(?:[^/\s]+/)?install\.sh"
)
_NVM_INSTALLER_PATTERN = re.compile(
    r"https://raw\.githubusercontent\.com/nvm-sh/nvm/"
    r"[^/\s]+/install\.sh"
)


def _inject_installer_environment(
    command: str,
    installer_pattern: re.Pattern[str],
    assignment: str,
) -> str:
    if assignment.split("=", 1)[0] + "=" in command:
        return command
    downloader = re.compile(
        r"\b(?:curl|wget)\b"
        r"(?=[^|;\n]*"
        + installer_pattern.pattern
        + r")"
    )
    return downloader.sub(
        lambda match: f"export {assignment} && {match.group(0)}",
        command,
    )


def normalize_github_proxy_prefix(prefix: str) -> str:
    """Validate and normalize a URL-prefix style GitHub download proxy."""
    normalized = prefix.strip()
    parsed = urlsplit(normalized)
    if parsed.scheme != "https" or not parsed.netloc:
        raise ValueError("GitHub proxy prefix must be an absolute HTTPS URL")
    if parsed.username or parsed.password:
        raise ValueError("GitHub proxy prefix must not contain credentials")
    if parsed.query or parsed.fragment:
        raise ValueError("GitHub proxy prefix must not contain query or fragment")
    if re.fullmatch(r"[A-Za-z0-9._~:/-]+", normalized) is None:
        raise ValueError("GitHub proxy prefix contains unsafe characters")
    return normalized.rstrip("/") + "/"


def rewrite_github_downloads(command: str, proxy_prefix: str) -> str:
    """Route GitHub download URLs in a shell command through a URL proxy."""
    prefix = normalize_github_proxy_prefix(proxy_prefix)
    rewritten = command
    github_base = prefix + "https://github.com"
    rewritten = _inject_installer_environment(
        rewritten,
        _UV_INSTALLER_PATTERN,
        f"UV_INSTALLER_GITHUB_BASE_URL={github_base}",
    )
    rewritten = _inject_installer_environment(
        rewritten,
        _NVM_INSTALLER_PATTERN,
        f"NVM_SOURCE={github_base}/nvm-sh/nvm.git",
    )

    def replace_origin(match: re.Match[str]) -> str:
        if (
            rewritten[max(0, match.start() - len(prefix)) : match.start()]
            == prefix
        ):
            return match.group(0)
        return prefix + match.group(0)

    rewritten = _GITHUB_DOWNLOAD_PATTERN.sub(replace_origin, rewritten)
    return rewritten


class GithubProxyMixin:
    """Apply the configured GitHub URL proxy to agent setup commands."""

    def __init__(
        self,
        *args: Any,
        github_proxy_prefix: str | None = None,
        **kwargs: Any,
    ) -> None:
        self._github_proxy_prefix = (
            normalize_github_proxy_prefix(github_proxy_prefix)
            if github_proxy_prefix is not None
            else None
        )
        super().__init__(*args, **kwargs)

    def _proxy_command(self, command: str) -> str:
        if self._github_proxy_prefix is None:
            return command
        return rewrite_github_downloads(command, self._github_proxy_prefix)

    def _proxy_environment(
        self,
        env: dict[str, str] | None,
    ) -> dict[str, str] | None:
        if self._github_proxy_prefix is None:
            return env
        github_base = self._github_proxy_prefix + "https://github.com"
        return {
            **(env or {}),
            "UV_INSTALLER_GITHUB_BASE_URL": github_base,
            "NVM_SOURCE": f"{github_base}/nvm-sh/nvm.git",
        }

    async def exec_as_root(
        self,
        environment: BaseEnvironment,
        command: str,
        **kwargs: Any,
    ) -> Any:
        env = self._proxy_environment(kwargs.pop("env", None))
        return await super().exec_as_root(
            environment,
            self._proxy_command(command),
            env=env,
            **kwargs,
        )

    async def exec_as_agent(
        self,
        environment: BaseEnvironment,
        command: str,
        **kwargs: Any,
    ) -> Any:
        env = self._proxy_environment(kwargs.pop("env", None))
        return await super().exec_as_agent(
            environment,
            self._proxy_command(command),
            env=env,
            **kwargs,
        )


class ProxyCodex(GithubProxyMixin, Codex):
    """Codex agent whose GitHub setup downloads can use a URL proxy."""


class ProxyOpenCode(GithubProxyMixin, OpenCode):
    """OpenCode agent whose GitHub setup downloads can use a URL proxy."""


class ProxyQwenCode(GithubProxyMixin, QwenCode):
    """Qwen Code agent whose GitHub setup downloads can use a URL proxy."""
