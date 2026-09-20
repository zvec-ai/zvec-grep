from __future__ import annotations

from .opencode import ResilientOpenCode
from .zvec_grep import ZvecGrepMixin


class ZvecOpenCode(ZvecGrepMixin, ResilientOpenCode):
    """OpenCode benchmark agent with the zvec-grep MCP server provisioned."""
