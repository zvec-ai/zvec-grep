#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
workspace_dir="$(cd -- "${script_dir}/.." && pwd)"
cd "${workspace_dir}"

cargo fmt --all --check
for package in zg-engine zg-daemon-protocol zg-cli zg-host-native zg-testkit zg-transport-mcp zg-daemon zg; do
  cargo check --package "${package}" --all-targets
done
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo metadata --no-deps --format-version 1 >/dev/null
node --test scripts/npm-local.test.mjs npm/meta/install.test.cjs
