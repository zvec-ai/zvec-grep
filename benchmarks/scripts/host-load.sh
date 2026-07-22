#!/usr/bin/env bash
# 在【Claude Code 所在的 podman 主机】上运行:把传过来的 tar 逐个 load 进 podman。
# 用法:  ./host-load.sh <tar 所在目录>
set -uo pipefail
DIR="${1:?需要 tar 目录}"
export DOCKER_HOST="unix:///run/user/$(id -u)/podman/podman.sock"

shopt -s nullglob
tars=("$DIR"/*.tar "$DIR"/*.tar.gz)
echo "发现 ${#tars[@]} 个 tar,开始 load ..."
n=0
for f in "${tars[@]}"; do
  n=$((n+1))
  echo "[$n/${#tars[@]}] load $f"
  case "$f" in
    *.tar.gz) gunzip -c "$f" | podman load ;;
    *)        podman load -i "$f" ;;
  esac
done

echo "=== 校验:本地 swebench eval 镜像数 ==="
podman images --format '{{.Repository}}:{{.Tag}}' | grep -c 'swebench/sweb.eval.x86_64' || true
