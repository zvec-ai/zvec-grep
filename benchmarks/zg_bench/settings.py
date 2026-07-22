from __future__ import annotations

CODEX_VERSION = "0.144.4"
QWEN_CODE_VERSION = "0.19.10"
OPENCODE_VERSION = "1.18.3"
QWEN_CODE_DASHSCOPE_MODEL = "qwen3.7-max"
QWEN_CODE_DASHSCOPE_BASE_URL = (
    "https://dashscope.aliyuncs.com/compatible-mode/v1"
)
OPENCODE_ALIYUN_GLM_MODEL = "aliyun-glm-5.2"
OPENCODE_ALIYUN_GLM_BASE_URL = (
    "https://dashscope.alibaba-inc.com/compatible-mode/v1"
)
ZVEC_GREP_PACKAGE = "@zvec/zvec-grep@0.1.5"
ZVEC_GREP_BINDING_PACKAGE = "@zvec/bindings-linux-x64@0.5.0"
ZVEC_GREP_EMBEDDING = "qwen/text-embedding-v4"
ZVEC_GREP_API_KEY_ENV_VARS = (
    "ZVEC_GREP_API_KEY",
    "DASHSCOPE_API_KEY",
    "QWEN_API_KEY",
)

# Agent installation can approach Harbor's default six-minute setup timeout on
# a fresh container. The zvec-grep profile also installs the tool and builds an
# index and, for supported agents, configures the MCP server before execution,
# so both profiles receive the same larger setup budget.
AGENT_SETUP_TIMEOUT_MULTIPLIER = "5"
