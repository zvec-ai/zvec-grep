#!/usr/bin/env bash
# 在本 podman 主机后台拉取全部 SWE-bench eval 镜像(经 registries.conf 的 docker.io 镜像站)。
# 磁盘守护:当 /home 可用空间低于 FLOOR_GB 时停手,绝不写满共享盘。断点续拉:已存在的镜像跳过。
set -uo pipefail
export DOCKER_HOST="unix:///run/user/$(id -u)/podman/podman.sock"

LIST="${1:?需要镜像清单}"
FLOOR_GB="${2:-250}"
FLOOR_BYTES=$(( FLOOR_GB * 1024 * 1024 * 1024 ))
STORE="$HOME/.local/share/containers/storage"
FAILED="${LIST%.txt}.pull-failed.txt"
: > "$FAILED"

mapfile -t IMAGES < "$LIST"
TOTAL=${#IMAGES[@]}
ok=0; skip=0; fail=0; i=0
echo "$(date '+%F %T') 开始:共 $TOTAL 个,磁盘下限 ${FLOOR_GB}GB"

for img in "${IMAGES[@]}"; do
  i=$((i+1))
  # 磁盘守护
  avail=$(df --output=avail -B1 "$STORE" 2>/dev/null | tail -1 | tr -dc '0-9')
  if [ -n "$avail" ] && [ "$avail" -lt "$FLOOR_BYTES" ]; then
    echo "$(date '+%F %T') !! 可用空间 $(( avail/1024/1024/1024 ))GB < 下限 ${FLOOR_GB}GB,停手。已成功 $ok,跳过 $skip,失败 $fail,处理到 $i/$TOTAL"
    exit 3
  fi
  if podman image exists "docker.io/$img" 2>/dev/null || podman image exists "$img" 2>/dev/null; then
    skip=$((skip+1)); continue
  fi
  echo "$(date '+%F %T') [$i/$TOTAL] pull $img  (avail $(( avail/1024/1024/1024 ))GB)"
  if podman pull --quiet "docker.io/$img" >/dev/null 2>&1; then
    ok=$((ok+1))
  else
    fail=$((fail+1)); echo "$img" >> "$FAILED"; echo "  !! 失败,记入 $(basename "$FAILED")"
  fi
done

echo "$(date '+%F %T') 完成:成功 $ok,跳过 $skip,失败 $fail(见 $FAILED)"
echo "本地 swebench eval 镜像总数: $(podman images --format '{{.Repository}}' 2>/dev/null | grep -c 'swebench/sweb.eval.x86_64')"
