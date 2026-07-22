#!/usr/bin/env bash
# 给 Harbor 缓存里所有 SWE-bench 任务包的 environment/Dockerfile 注入受限网络修复。
# 幂等:已注入的跳过。复用 smoke 已验证(reward=1.0)的两处改法:
#   1) FROM 行后插入 uv/pip 索引 ENV(指向 aliyun 镜像;容器内 pypi.org 被墙,评分 uv run 需要)
#   2) uv-install RUN 前置 UV_INSTALLER_GITHUB_BASE_URL=ghfast 代理(容器内 github.com 被墙)
#
# 用法:  ./inject-network-fixes.sh [Harbor缓存packages根目录]
# 默认根目录: ~/.cache/harbor/tasks/packages
set -uo pipefail

ROOT="${1:-$HOME/.cache/harbor/tasks/packages}"
GH_PROXY="https://ghfast.top/https://github.com"
PY_INDEX="https://mirrors.aliyun.com/pypi/simple/"

mapfile -t DFS < <(grep -rlE '^\s*FROM\s+swebench/sweb\.eval\.x86_64\.' "$ROOT" \
  --include=Dockerfile 2>/dev/null | grep '/environment/Dockerfile$')

echo "在 $ROOT 下找到 ${#DFS[@]} 个 swebench 任务 Dockerfile"
patched=0; skipped=0
for df in "${DFS[@]}"; do
  if grep -q 'UV_INSTALLER_GITHUB_BASE_URL' "$df"; then
    skipped=$((skipped+1)); continue
  fi
  python3 - "$df" "$GH_PROXY" "$PY_INDEX" <<'PY'
import re, sys
df, gh, idx = sys.argv[1], sys.argv[2], sys.argv[3]
text = open(df, encoding="utf-8").read()
lines = text.splitlines(keepends=True)
out = []
env_block = (
    f'ENV UV_DEFAULT_INDEX={idx}\n'
    f'ENV UV_INDEX_URL={idx}\n'
    f'ENV PIP_INDEX_URL={idx}\n'
)
from_done = False
for ln in lines:
    out.append(ln)
    # 在第一条未注释的 FROM swebench 行之后插入索引 ENV
    if (not from_done) and re.match(r'\s*FROM\s+swebench/sweb\.eval\.x86_64\.', ln):
        if not ln.endswith("\n"):
            out[-1] = ln + "\n"
        out.append("\n# --- injected: CN-restricted network fixes ---\n")
        out.append(env_block)
        from_done = True
text2 = "".join(out)
# uv 安装 RUN 前置 github 代理(仅当尚未前置)
text2 = re.sub(
    r'RUN\s+curl\s+-LsSf\s+(https://astral\.sh/uv/[^\s|]+/install\.sh)\s*\|\s*sh',
    r'RUN export UV_INSTALLER_GITHUB_BASE_URL="%s"; curl -LsSf \1 | sh' % gh,
    text2,
)
open(df, "w", encoding="utf-8").write(text2)
PY
  patched=$((patched+1))
done
echo "完成:注入 $patched 个,跳过(已注入)$skipped 个"
