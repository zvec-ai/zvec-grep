#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "${script_dir}/../.."

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "CUDA builds are supported only on Linux" >&2
  exit 2
fi
commands=(cargo patchelf readelf python3)
if [[ "${LLAMA_CPP_BUILD_MODE:-auto}" != prebuilt ]]; then
  commands+=(cmake nvcc)
fi
for command in "${commands[@]}"; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command is unavailable: ${command}" >&2
    exit 2
  fi
done

if [[ -z "${CMAKE_CUDA_ARCHITECTURES:-}" ]]; then
  # Distribution builds must not inherit the architecture of whichever GPU
  # happens to be attached to the builder. Keep native code for the deployed
  # x86_64 generations and PTX for forward compatibility. CMake's all-major
  # setting is intentionally avoided: newer CUDA expands it to sm_120, while
  # llama.cpp's NVFP4 kernels currently contain instructions that ptxas cannot
  # assemble for that target.
  CMAKE_CUDA_ARCHITECTURES="75-real;80-real;86-real;89-real;90-real;90-virtual"
  export CMAKE_CUDA_ARCHITECTURES
fi

if [[ -z "${CMAKE_BUILD_PARALLEL_LEVEL:-}" ]]; then
  CMAKE_BUILD_PARALLEL_LEVEL="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"
  export CMAKE_BUILD_PARALLEL_LEVEL
fi

cuda_toolkit_version=13.1
if [[ "${LLAMA_CPP_BUILD_MODE:-auto}" != prebuilt ]]; then
  cuda_toolkit_version="$(nvcc --version | sed -n 's/.*release \([0-9][0-9.]*\).*/\1/p' | tail -n 1)"
fi
if [[ "${cuda_toolkit_version}" != "13.1" ]]; then
  echo "the primary GGUF backend requires CUDA 13.1 (found ${cuda_toolkit_version})" >&2
  exit 2
fi
echo "GGUF toolkit: ${cuda_toolkit_version}; ONNX Runtime: official CUDA 12 dynamic runtime"
echo "CUDA architectures: ${CMAKE_CUDA_ARCHITECTURES}"

target_dir="${CARGO_TARGET_DIR:-target}"
if [[ "${target_dir}" != /* ]]; then
  target_dir="${PWD}/${target_dir}"
fi
# The pinned sys crate fingerprints native options and invalidates its own
# CMake state. Do not discard unrelated Rust output or the verified SDK cache.
mkdir -p "${target_dir}"
cargo build --locked --release --package zg --features cuda \
  --message-format=json-render-diagnostics > "${target_dir}/cuda-build.json"

profile_dir="${target_dir}/release"
bundle_dir="${profile_dir}/zg-cuda"
staging_dir="$(mktemp -d "${profile_dir}/.zg-cuda.XXXXXX")"
trap 'rm -rf -- "${staging_dir}"' EXIT
install -d "${staging_dir}/bin" "${staging_dir}/lib"

install -m 0755 "${profile_dir}/zg" "${staging_dir}/bin/zg"

python3 scripts/cuda/runtime.py --cargo-messages "${target_dir}/cuda-build.json" \
  --destination "${staging_dir}/lib"

python3 scripts/cuda/prepare-onnxruntime.py --platform linux \
  --cache-dir "${target_dir}/ort-cuda12" --destination "${staging_dir}/lib"

if [[ ! -f "${profile_dir}/libzvec_c_api.so" ]]; then
  echo "unable to locate libzvec_c_api.so" >&2
  exit 1
fi
cp -L --preserve=mode,timestamps -- "${profile_dir}/libzvec_c_api.so" "${staging_dir}/lib/"

patchelf --set-rpath '$ORIGIN/../lib' "${staging_dir}/bin/zg"
while IFS= read -r -d '' file; do
  if readelf -h "${file}" >/dev/null 2>&1; then
    patchelf --set-rpath '$ORIGIN' "${file}"
  fi
done < <(find "${staging_dir}/lib" -maxdepth 1 -type f -print0)

chmod 0755 "${staging_dir}"
# SDK staging includes both plugins. Source builds may defer the fallback to
# another toolchain. --primary-only always defers the complete bundle audit.
if [[ "${1:-}" != "--primary-only" ]]; then
  if [[ ! -f "${staging_dir}/lib/cuda12/libggml-cuda.so" ]]; then
    python3 scripts/cuda/build-fallback.py --bundle "${staging_dir}" \
      --cargo-messages "${target_dir}/cuda-build.json" \
      --cuda-root "${CUDA12_ROOT:-/usr/local/cuda-12.4}"
  fi
  bash scripts/cuda/check.sh "${staging_dir}"
else
  bundle_dir="${profile_dir}/zg-cuda-staging"
fi
rm -rf -- "${bundle_dir}"
mv -- "${staging_dir}" "${bundle_dir}"
trap - EXIT
echo "CUDA bundle: ${bundle_dir}"
