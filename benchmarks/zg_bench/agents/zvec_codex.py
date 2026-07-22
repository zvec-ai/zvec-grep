from __future__ import annotations

from harbor.agents.installed.codex import Codex

from .zvec_grep import ZvecGrepMixin


class ZvecCodex(ZvecGrepMixin, Codex):
    """Codex benchmark agent with the zvec-grep MCP server provisioned."""
