from __future__ import annotations

import os
from pathlib import Path

CODEX_VERSION = "0.144.4"
CLAUDE_CODE_VERSION = "2.1.212"
CLAUDE_OPUS_5_MODEL = "claude-opus-5"
CLAUDE_CODE_REASONING_EFFORT = "high"
CLAUDE_CODE_MAX_BUDGET_USD = 4.0
OPENCODE_VERSION = "1.18.4"
DASHSCOPE_QWEN_3_7_MAX_MODEL = "qwen3.7-max"
OPENCODE_ALIYUN_GLM_MODEL = "aliyun-glm-5.2"
OPENCODE_ALIYUN_GLM_MODEL_ID = "glm-5.2"
OPENCODE_CUSTOM_GLM_MODEL = "custom-openai/glm-5.2"
OPENCODE_CUSTOM_GLM_MODEL_ID = "glm-5.2"
OPENCODE_CUSTOM_GLM_BASE_URL = (
    "https://llm-67x4s810wr6kl2i4.cn-beijing.maas.aliyuncs.com/compatible-mode/v1"
)
OPENCODE_ALIYUN_QWEN_MODEL = DASHSCOPE_QWEN_3_7_MAX_MODEL
OPENCODE_ALIYUN_QWEN_MODEL_ID = DASHSCOPE_QWEN_3_7_MAX_MODEL
OPENCODE_DASHSCOPE_BASE_URL = "https://dashscope.aliyuncs.com/compatible-mode/v1"
OPENCODE_OPENAI_COMPATIBLE_PACKAGE = "@ai-sdk/openai-compatible"
ZVEC_GREP_PACKAGE = "@zvec/zvec-grep@0.1.6-alpha.3"
ZVEC_GREP_BINDING_PACKAGE = "@zvec/bindings-linux-x64@0.5.0"
ZVEC_GREP_EMBEDDING = "qwen/qwen3.7-text-embedding"
ZVEC_GREP_EMBEDDING_ENDPOINT = (
    "https://llm-67x4s810wr6kl2i4.cn-beijing.maas.aliyuncs.com/"
    "compatible-mode/v1/embeddings"
)
ZVEC_GREP_INDEX_SEED_ENV = "ZG_BENCH_INDEX_SEED_DIR"
ZVEC_GREP_INDEX_SEED_FORMAT_VERSION = 1
ZVEC_GREP_API_KEY_ENV_VARS = (
    "ZVEC_GREP_API_KEY",
    "DASHSCOPE_API_KEY",
    "QWEN_API_KEY",
)


def resolve_zvec_grep_index_seed_dir(value: str | None) -> Path | None:
    """Resolve a host-only seed directory while rejecting broad targets."""
    if value is None or not value.strip():
        return None
    candidate = Path(value).expanduser().resolve()
    forbidden = {
        Path("/"),
        Path.home().resolve(),
        Path.cwd().resolve(),
    }
    github_workspace = os.environ.get("GITHUB_WORKSPACE", "").strip()
    if github_workspace:
        forbidden.add(Path(github_workspace).expanduser().resolve())
    if candidate in forbidden:
        raise ValueError(
            "zvec-grep index seed directory must be a dedicated cache directory, "
            f"not {candidate}"
        )
    return candidate


# Agent installation can approach Harbor's default six-minute setup timeout on
# a fresh container. The zvec-grep profile also installs the tool and builds an
# index and, for supported agents, configures the MCP server before execution,
# so both profiles receive the same larger setup budget.
AGENT_SETUP_TIMEOUT_MULTIPLIER = "5"
