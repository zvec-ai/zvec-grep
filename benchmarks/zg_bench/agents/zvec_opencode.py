from __future__ import annotations

from .opencode_acp import OpenCodeACP
from .zvec_grep import ZvecGrepMixin


class ZvecOpenCode(ZvecGrepMixin, OpenCodeACP):
    """OpenCode benchmark agent with the zvec-grep MCP server provisioned."""
