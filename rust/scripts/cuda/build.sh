#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "${script_dir}/../.."

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "CUDA builds are supported only on Linux" >&2
  exit 2
fi
for command in cargo cmake nvcc patchelf readelf python3; do
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

cuda_toolkit_version="$(nvcc --version | sed -n 's/.*release \([0-9][0-9.]*\).*/\1/p' | tail -n 1)"
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
build_config="cuda_toolkit=${cuda_toolkit_version};ort=1.28.0-cuda12-dynamic;architectures=${CMAKE_CUDA_ARCHITECTURES}"
build_config_file="${target_dir}/.zvec-grep-cuda-build-config"
previous_build_config="$(sed -n '1p' "${build_config_file}" 2>/dev/null || true)"
if [[ "${previous_build_config}" != "${build_config}" && -f "${target_dir}/CACHEDIR.TAG" ]]; then
  # A failed custom build is not necessarily removed by package-scoped
  # `cargo clean`, leaving its CMake cache configured for the old architecture.
  echo "CUDA build configuration changed; rebuilding the target directory"
  cargo clean --target-dir "${target_dir}"
fi

cargo build --locked --release --package zg --features cuda
printf '%s\n' "${build_config}" > "${build_config_file}"

profile_dir="${target_dir}/release"
bundle_dir="${profile_dir}/zg-cuda"
staging_dir="$(mktemp -d "${profile_dir}/.zg-cuda.XXXXXX")"
trap 'rm -rf -- "${staging_dir}"' EXIT
install -d "${staging_dir}/bin" "${staging_dir}/lib"

install -m 0755 "${profile_dir}/zg" "${staging_dir}/bin/zg"

cuda_backend="$(find "${profile_dir}/build" -path '*/llama-cpp-sys-2-*/out/backends/libggml-cuda.so*' -type f -printf '%T@ %p\n' \
  | sort -nr | sed -n '1s/^[^ ]* //p')"
if [[ -z "${cuda_backend}" ]]; then
  echo "unable to locate the llama.cpp CUDA backend module" >&2
  exit 1
fi
backend_dir="$(dirname -- "${cuda_backend}")"
llama_out_dir="$(dirname -- "${backend_dir}")"

shopt -s nullglob
llama_backend_libraries=("${backend_dir}"/libggml-*.so*)
if (( ${#llama_backend_libraries[@]} == 0 )); then
  echo "llama.cpp shared libraries are incomplete" >&2
  exit 1
fi

copy_core_library() {
  local stem="$1"
  local source=""
  local candidate
  for candidate in "${llama_out_dir}"/lib*/"${stem}".so*; do
    if [[ -f "${candidate}" && ! -L "${candidate}" ]]; then
      source="${candidate}"
      break
    fi
  done
  if [[ -z "${source}" ]]; then
    echo "unable to locate the real shared library for ${stem}" >&2
    exit 1
  fi
  local soname
  soname="$(readelf -d "${source}" | sed -n 's/.*(SONAME).*\[\([^]]*\)\].*/\1/p' | head -n 1)"
  if [[ -z "${soname}" || "${soname}" == */* ]]; then
    echo "invalid or missing SONAME for ${source}: ${soname}" >&2
    exit 1
  fi
  cp -L --preserve=mode,timestamps -- "${source}" "${staging_dir}/lib/${soname}"
}

copy_core_library libllama
copy_core_library libggml
copy_core_library libggml-base
cp -L --preserve=mode,timestamps -- "${llama_backend_libraries[@]}" "${staging_dir}/lib/"

python3 scripts/cuda/prepare-onnxruntime.py --platform linux \
  --cache-dir "${target_dir}/ort-cuda12" --destination "${staging_dir}/lib"

if [[ ! -f "${profile_dir}/libzvec_c_api.so" ]]; then
  echo "unable to locate libzvec_c_api.so" >&2
  exit 1
fi
cp -L --preserve=mode,timestamps -- "${profile_dir}/libzvec_c_api.so" "${staging_dir}/lib/"
if [[ -d "${profile_dir}/data" ]]; then
  cp -a -- "${profile_dir}/data" "${staging_dir}/lib/"
fi

patchelf --set-rpath '$ORIGIN/../lib' "${staging_dir}/bin/zg"
while IFS= read -r -d '' file; do
  if readelf -h "${file}" >/dev/null 2>&1; then
    patchelf --set-rpath '$ORIGIN' "${file}"
  fi
done < <(find "${staging_dir}/lib" -maxdepth 1 -type f -print0)

chmod 0755 "${staging_dir}"
rm -rf -- "${bundle_dir}"
mv -- "${staging_dir}" "${bundle_dir}"
trap - EXIT

# CI can build the fallback in a separate CUDA 12.4 container before auditing.
if [[ "${1:-}" != "--primary-only" ]]; then
  python3 scripts/cuda/build-fallback.py --bundle "${bundle_dir}" \
    --cuda-root "${CUDA12_ROOT:-/usr/local/cuda-12.4}"
  bash scripts/cuda/check.sh "${bundle_dir}"
fi
echo "CUDA bundle: ${bundle_dir}"
