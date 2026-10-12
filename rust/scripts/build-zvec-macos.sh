#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "x86_64" ]]; then
  echo "zvec source builds require an Intel macOS runner" >&2
  exit 1
fi

# This native revision is recorded by zvec-rust 0.7.2's vendor/zvec submodule.
readonly ZVEC_REVISION="1ab7975dfc2d2160054bafff614831b7099cd930"
readonly ZVEC_TEMP_DIR="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/zg-zvec-XXXXXX")"
readonly ZVEC_SOURCE_DIR="$ZVEC_TEMP_DIR/source"
readonly ZVEC_BUILD_DIR="$ZVEC_TEMP_DIR/build"
readonly ZVEC_RUNTIME_DIR="$PWD/target/zvec-runtime"

git init "$ZVEC_SOURCE_DIR"
git -C "$ZVEC_SOURCE_DIR" remote add origin https://github.com/alibaba/zvec.git
git -C "$ZVEC_SOURCE_DIR" fetch --depth 1 origin "$ZVEC_REVISION"
git -C "$ZVEC_SOURCE_DIR" checkout --detach FETCH_HEAD
git -C "$ZVEC_SOURCE_DIR" submodule update --init --recursive --depth 1

# Match the upstream macOS SDK toolchain without replacing the runner's CMake.
python3 -m venv "$ZVEC_TEMP_DIR/tools"
"$ZVEC_TEMP_DIR/tools/bin/python" -m pip install cmake==3.30.0
"$ZVEC_TEMP_DIR/tools/bin/cmake" -S "$ZVEC_SOURCE_DIR" -B "$ZVEC_BUILD_DIR" \
  -G "Unix Makefiles" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_OSX_ARCHITECTURES=x86_64 \
  -DCMAKE_OSX_DEPLOYMENT_TARGET=14.0 \
  -DBUILD_C_BINDINGS=ON \
  -DBUILD_PYTHON_BINDINGS=OFF \
  -DBUILD_TOOLS=OFF
"$ZVEC_TEMP_DIR/tools/bin/cmake" --build "$ZVEC_BUILD_DIR" \
  --config Release --parallel 4 --target zvec_c_api

mkdir -p "$ZVEC_RUNTIME_DIR/data/jieba_dict"
cp -L "$ZVEC_BUILD_DIR/lib/"*.dylib "$ZVEC_RUNTIME_DIR/"
cp "$ZVEC_SOURCE_DIR/thirdparty/cppjieba/cppjieba-5.6.7/dict/jieba.dict.utf8" \
  "$ZVEC_SOURCE_DIR/thirdparty/cppjieba/cppjieba-5.6.7/dict/hmm_model.utf8" \
  "$ZVEC_RUNTIME_DIR/data/jieba_dict/"
for library in "$ZVEC_RUNTIME_DIR/"*.dylib; do
  install_name_tool -id "@rpath/$(basename "$library")" "$library"
  otool -L "$library"
done
test -f "$ZVEC_RUNTIME_DIR/libzvec_c_api.dylib"
