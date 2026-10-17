#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "CUDA linkage audit is supported only on Linux" >&2
  exit 2
fi

bundle="${1:-target/release/zg-cuda}"
binary="${bundle}/bin/zg"
lib_dir="${bundle}/lib"
if [[ ! -x "${binary}" ]]; then
  echo "zg binary is missing or not executable: ${binary}" >&2
  exit 2
fi
if ! command -v readelf >/dev/null 2>&1; then
  echo "readelf is required (install binutils)" >&2
  exit 2
fi

needed() {
  readelf -d "$1" | sed -n 's/.*Shared library: \[\([^]]*\)\].*/\1/p'
}

require_file() {
  local pattern="$1"
  compgen -G "${bundle}/${pattern}" >/dev/null || {
    echo "missing packaged shared library: ${pattern}" >&2
    exit 1
  }
}

require_needed() {
  local dependencies="$1"
  local prefix="$2"
  if ! awk -v prefix="${prefix}" '$0 == prefix || index($0, prefix ".") == 1 { found = 1 } END { exit !found }' <<<"${dependencies}"; then
    echo "missing expected dynamic dependency: ${prefix}" >&2
    exit 1
  fi
}

for pattern in \
  'lib/libzvec_c_api.so' \
  'lib/libllama.so*' \
  'lib/libggml.so*' \
  'lib/libggml-base.so*' \
  'lib/libggml-cpu*.so*' \
  'lib/libggml-cuda.so*' \
  'lib/cuda12/libggml-cuda.so' \
  'lib/libonnxruntime.so.1' \
  'lib/libonnxruntime_providers_shared.so' \
  'lib/libonnxruntime_providers_cuda.so'; do
  require_file "${pattern}"
done

if find "${bundle}" -type l -print -quit | grep -q .; then
  echo "CUDA bundle must not contain symbolic links" >&2
  find "${bundle}" -maxdepth 1 -type l -print >&2
  exit 1
fi

for stem in libllama libggml libggml-base libonnxruntime; do
  count="$(find "${lib_dir}" -maxdepth 1 -type f -name "${stem}.so*" | wc -l)"
  if [[ "${count}" -ne 1 ]]; then
    echo "expected one canonical ${stem} shared library, found ${count}" >&2
    exit 1
  fi
done

binary_needed="$(needed "${binary}")"
require_needed "${binary_needed}" libzvec_c_api.so
require_needed "${binary_needed}" libllama.so
if grep -Eq '^lib(cuda|cudart|cublas|cudnn)' <<<"${binary_needed}"; then
  echo "zg must not require CUDA libraries at process startup:" >&2
  grep -E '^lib(cuda|cudart|cublas|cudnn)' <<<"${binary_needed}" >&2
  exit 1
fi
if ! readelf -d "${binary}" | grep -Fq '$ORIGIN/../lib'; then
  echo "zg is missing the bin-to-lib runtime search path" >&2
  exit 1
fi

while IFS= read -r -d '' cpu_file; do
  cpu_needed="$(needed "${cpu_file}")"
  if grep -Eq '^lib(cuda|cudart|cublas|cudnn)' <<<"${cpu_needed}"; then
    echo "CPU/startup library must not require CUDA: ${cpu_file}" >&2
    grep -E '^lib(cuda|cudart|cublas|cudnn)' <<<"${cpu_needed}" >&2
    exit 1
  fi
done < <(find "${lib_dir}" -maxdepth 1 -type f \( \
  -name 'libzvec_c_api.so' -o \
  -name 'libllama.so*' -o \
  -name 'libggml.so*' -o \
  -name 'libggml-base.so*' -o \
  -name 'libggml-cpu*.so*' -o \
  -name 'libonnxruntime.so.1' -o \
  -name 'libonnxruntime_providers_shared.so' \
\) -print0)

cuda_backend="$(find "${lib_dir}" -maxdepth 1 -name 'libggml-cuda.so*' -type f -print -quit)"
cuda_needed="$(needed "${cuda_backend}")"
for library in libcudart.so.13 libcublas.so.13 libcuda.so.1; do
  require_needed "${cuda_needed}" "${library}"
done

fallback_needed="$(needed "${lib_dir}/cuda12/libggml-cuda.so")"
require_needed "${fallback_needed}" libcublas.so.12
require_needed "${fallback_needed}" libggml-base.so
if [[ "$(find "${lib_dir}/cuda12" -type f | wc -l)" -ne 1 ]]; then
  echo "cuda12 must contain only the fallback CUDA plugin" >&2
  exit 1
fi
if ! readelf -d "${lib_dir}/cuda12/libggml-cuda.so" | grep -Fq '$ORIGIN/..'; then
  echo "CUDA 12 plugin must resolve the shared core from its parent" >&2
  exit 1
fi

provider_needed="$(needed "${lib_dir}/libonnxruntime_providers_cuda.so")"
require_needed "${provider_needed}" libcublas.so.12
require_needed "${provider_needed}" libcudart.so.12
# ORT 1.28 loads cuDNN/cuFFT on demand; they need not appear in DT_NEEDED.
if ! grep -Eq '^lib(cuda|cudart|cublas|cudnn)' <<<"${provider_needed}"; then
  echo "ONNX Runtime CUDA provider has no CUDA runtime dependency" >&2
  exit 1
fi
if find "${bundle}" -type f -name 'libcu*.so*' -print -quit | grep -q .; then
  echo "NVIDIA libraries must remain host dependencies" >&2
  exit 1
fi

echo "CUDA bundle linkage policy passed for ${bundle}"
echo "zg dependencies:"
echo "${binary_needed}"
echo "llama.cpp CUDA backend dependencies:"
echo "${cuda_needed}"
