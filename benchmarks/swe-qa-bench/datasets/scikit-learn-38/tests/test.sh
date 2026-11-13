#!/usr/bin/env bash
set -euo pipefail

mkdir -p /logs/verifier
if [[ ! -d /app ]]; then
  echo 0 > /logs/verifier/reward.txt
  exit 1
fi

echo 1 > /logs/verifier/reward.txt
