#!/usr/bin/env bash
# 磁盘看门狗:每 60s 检查一次;/home 可用空间低于 FLOOR_GB 时,杀掉 harbor 全量 run,保护共享盘。
set -uo pipefail
FLOOR_GB="${1:-200}"
FLOOR_BYTES=$(( FLOOR_GB * 1024 * 1024 * 1024 ))
STORE="$HOME/.local/share/containers/storage"
while true; do
  avail=$(df --output=avail -B1 "$STORE" 2>/dev/null | tail -1 | tr -dc '0-9')
  if [ -n "$avail" ] && [ "$avail" -lt "$FLOOR_BYTES" ]; then
    echo "$(date '+%F %T') !! 可用 $(( avail/1024/1024/1024 ))GB < ${FLOOR_GB}GB,停 harbor 全量 run 保护共享盘"
    pkill -f "harbor run" 2>/dev/null
    pkill -f "docker-compose" 2>/dev/null
    exit 3
  fi
  sleep 60
done
