from __future__ import annotations

from harbor.agents.installed.qwen_code import QwenCode

from ..github_proxy import GithubProxyMixin
from .zvec_grep import ZvecGrepMixin


class ZvecQwenCode(GithubProxyMixin, ZvecGrepMixin, QwenCode):
    """Harbor's Qwen Code agent with zvec-grep provisioned before execution."""
