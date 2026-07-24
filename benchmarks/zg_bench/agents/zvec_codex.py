from __future__ import annotations

from harbor.agents.installed.codex import Codex

from ..github_proxy import GithubProxyMixin
from .zvec_grep import ZvecGrepMixin


class ZvecCodex(GithubProxyMixin, ZvecGrepMixin, Codex):
    """Codex benchmark agent with the zvec-grep MCP server provisioned."""
