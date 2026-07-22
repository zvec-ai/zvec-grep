#!/usr/bin/env bash
# 启动全 500 baseline(直接 harbor,不带 --include-task-name = 整套数据集)。
set -uo pipefail
cd /home/cuiyushuai.cys/workspace/zvec-grep-main/benchmarks
export PATH="$HOME/.local/bin:$PATH"
export DOCKER_HOST="unix:///run/user/$(id -u)/podman/podman.sock"

TS=$(date +%Y%m%d-%H%M%S)
JN="baseline-full-$TS"
LOG="runs/$JN.log"
echo "$JN" > runs/.last-full-job
echo "$LOG" > runs/.last-full-log

exec uv run harbor run --dataset swe-bench/swe-bench-verified@2 \
  --agent claude-code --model claude-opus-4-8 --env docker \
  --n-attempts 1 --max-retries 2 --n-concurrent 4 \
  --agent-setup-timeout-multiplier 5 \
  --jobs-dir runs --job-name "$JN" > "$LOG" 2>&1
