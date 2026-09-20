from __future__ import annotations

from harbor.agents.installed.claude_code import ClaudeCode
from harbor.models.trial.paths import EnvironmentPaths

from .zvec_grep import ZvecGrepMixin


class ZvecClaudeCode(ZvecGrepMixin, ClaudeCode):
    """Claude Code benchmark agent with the zvec-grep MCP server provisioned."""

    def _mcp_install_environment(self) -> dict[str, str]:
        # Harbor isolates each Claude Code run under /logs/agent/sessions.
        # Install MCP settings and CLAUDE.md into the same directory that the
        # native adapter passes as CLAUDE_CONFIG_DIR when it launches Claude.
        return {
            "CLAUDE_CONFIG_DIR": (
                EnvironmentPaths.agent_dir / "sessions"
            ).as_posix()
        }
