#!/usr/bin/env bash
# 在【联网 Docker 服务器 (iZ)】上运行:按清单拉取 SWE-bench eval 镜像并保存为 tar。
# 用法:
#   ./iz-pull-save.sh <镜像清单文件> <输出目录> [起始行] [结束行]
# 例:
#   ./iz-pull-save.sh swebench-verified-images.txt ./swebench-images          # 全部 500
#   ./iz-pull-save.sh swebench-verified-images.txt ./swebench-images 1 1      # 只第 1 个
#   ./iz-pull-save.sh swebench-verified-images.txt ./swebench-images 1 20     # 前 20 个
set -uo pipefail

LIST="${1:?需要镜像清单文件}"
OUT="${2:?需要输出目录}"
START="${3:-1}"
END="${4:-0}"   # 0 = 到末尾

mkdir -p "$OUT"
: > "$OUT/pull-failed.txt"

mapfile -t IMAGES < "$LIST"
TOTAL=${#IMAGES[@]}
[ "$END" -eq 0 ] && END=$TOTAL

echo "清单共 $TOTAL 个镜像;处理区间 [$START, $END]"
i=0
for img in "${IMAGES[@]}"; do
  i=$((i+1))
  { [ "$i" -lt "$START" ] || [ "$i" -gt "$END" ]; } && continue
  fname="$OUT/$(echo "$img" | tr '/:' '__').tar"
  if [ -s "$fname" ]; then echo "[$i/$TOTAL] 跳过(已存在) $fname"; continue; fi
  echo "[$i/$TOTAL] pull  $img"
  if ! docker pull --platform linux/amd64 "$img"; then
    echo "$img" >> "$OUT/pull-failed.txt"; echo "  !! pull 失败,记入 pull-failed.txt"; continue
  fi
  echo "[$i/$TOTAL] save  -> $fname"
  if ! docker save "$img" -o "$fname"; then
    echo "$img" >> "$OUT/pull-failed.txt"; rm -f "$fname"; echo "  !! save 失败"; continue
  fi
done

echo "完成。失败清单:$OUT/pull-failed.txt ($(wc -l < "$OUT/pull-failed.txt") 个)"
echo "已生成 tar:$(ls "$OUT"/*.tar 2>/dev/null | wc -l) 个,总大小:$(du -sh "$OUT" 2>/dev/null | cut -f1)"
