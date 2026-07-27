from __future__ import annotations

from harbor.agents.installed.opencode import OpenCode

from ..github_proxy import GithubProxyMixin
from .zvec_grep import ZvecGrepMixin


class ZvecOpenCode(ZvecGrepMixin, GithubProxyMixin, OpenCode):
    """OpenCode benchmark agent with the zvec-grep MCP server provisioned."""
