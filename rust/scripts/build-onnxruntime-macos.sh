#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "x86_64" ]]; then
  echo "ONNX Runtime source builds require an Intel macOS runner" >&2
  exit 1
fi

readonly ORT_VERSION="1.28.0"
readonly ORT_TEMP_DIR="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/zg-ort-XXXXXX")"
readonly ORT_SOURCE_DIR="$ORT_TEMP_DIR/source"
readonly ORT_BUILD_DIR="$ORT_TEMP_DIR/build"
readonly ORT_RUNTIME_DIR="$PWD/target/ort-runtime"

# ort no longer supplies Intel macOS binaries. Build the same runtime generation
# with CoreML enabled and stage relocatable shared libraries for npm packaging.
git clone --depth 1 --branch "v$ORT_VERSION" --recursive \
  https://github.com/microsoft/onnxruntime.git "$ORT_SOURCE_DIR"
python3 "$ORT_SOURCE_DIR/tools/ci_build/build.py" \
  --build_dir "$ORT_BUILD_DIR" \
  --config Release --update --build --build_shared_lib \
  --parallel 4 --skip_tests --compile_no_warning_as_error --use_coreml \
  --cmake_extra_defines \
    CMAKE_OSX_ARCHITECTURES=x86_64 \
    CMAKE_OSX_DEPLOYMENT_TARGET=13.3 \
    onnxruntime_BUILD_UNIT_TESTS=OFF \
    CMAKE_POLICY_VERSION_MINIMUM=3.5

mkdir -p "$ORT_RUNTIME_DIR"
cp -L "$ORT_BUILD_DIR/Release/"*.dylib "$ORT_RUNTIME_DIR/"
for library in "$ORT_RUNTIME_DIR/"*.dylib; do
  install_name_tool -id "@rpath/$(basename "$library")" "$library"
  otool -L "$library"
done
