#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "x86_64" ]]; then
  echo "The Intel macOS zvec SDK requires an Intel macOS runner" >&2
  exit 1
fi

# Match the pinned zvec-rust dependency and verify the upstream release asset.
readonly ZVEC_VERSION="0.8.0"
readonly ZVEC_SHA256="bace0c233ce43900464329073979c565832f6af5b23d086656656b88c6ba0ff3"
readonly ZVEC_TEMP_DIR="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/zg-zvec-XXXXXX")"
readonly ZVEC_ARCHIVE="$ZVEC_TEMP_DIR/zvec-prebuilt-x86_64-apple-darwin.tar.gz"
readonly ZVEC_RUNTIME_DIR="$PWD/target/zvec-runtime"
trap 'rm -rf "$ZVEC_TEMP_DIR"' EXIT

curl --fail --location --retry 3 --output "$ZVEC_ARCHIVE" \
  "https://github.com/zvec-ai/zvec-rust/releases/download/v$ZVEC_VERSION/$(basename "$ZVEC_ARCHIVE")"
echo "$ZVEC_SHA256  $ZVEC_ARCHIVE" | shasum -a 256 --check

mkdir -p "$ZVEC_RUNTIME_DIR"
tar -xzf "$ZVEC_ARCHIVE" -C "$ZVEC_RUNTIME_DIR"
test "$(cat "$ZVEC_RUNTIME_DIR/TARGET")" = "x86_64-apple-darwin"
test -f "$ZVEC_RUNTIME_DIR/libzvec_c_api.dylib"
for library in "$ZVEC_RUNTIME_DIR/"*.dylib; do
  lipo "$library" -verify_arch x86_64
  install_name_tool -id "@rpath/$(basename "$library")" "$library"
  otool -L "$library"
done
