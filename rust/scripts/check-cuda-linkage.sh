#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "CUDA linkage audit is supported only on Linux" >&2
  exit 2
fi

bundle="${1:-target/release/zg-cuda}"
binary="${bundle}/zg"
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
  'libzvec_c_api.so' \
  'libllama.so*' \
  'libggml.so*' \
  'libggml-base.so*' \
  'libggml-cpu*.so*' \
  'libggml-cuda.so*' \
  'libonnxruntime_providers_shared.so' \
  'libonnxruntime_providers_cuda.so'; do
  require_file "${pattern}"
done

binary_needed="$(needed "${binary}")"
require_needed "${binary_needed}" libzvec_c_api.so
require_needed "${binary_needed}" libllama.so
if grep -Eq '^lib(cuda|cudart|cublas|cudnn)' <<<"${binary_needed}"; then
  echo "zg must not require CUDA libraries at process startup:" >&2
  grep -E '^lib(cuda|cudart|cublas|cudnn)' <<<"${binary_needed}" >&2
  exit 1
fi
if ! readelf -d "${binary}" | grep -Fq '$ORIGIN'; then
  echo "zg is missing an origin-relative runtime search path" >&2
  exit 1
fi

while IFS= read -r -d '' cpu_file; do
  cpu_needed="$(needed "${cpu_file}")"
  if grep -Eq '^lib(cuda|cudart|cublas|cudnn)' <<<"${cpu_needed}"; then
    echo "CPU/startup library must not require CUDA: ${cpu_file}" >&2
    grep -E '^lib(cuda|cudart|cublas|cudnn)' <<<"${cpu_needed}" >&2
    exit 1
  fi
done < <(find "${bundle}" -maxdepth 1 -type f \( \
  -name 'zg' -o \
  -name 'libzvec_c_api.so' -o \
  -name 'libllama.so*' -o \
  -name 'libggml.so*' -o \
  -name 'libggml-base.so*' -o \
  -name 'libggml-cpu*.so*' -o \
  -name 'libonnxruntime_providers_shared.so' \
\) -print0)

cuda_backend="$(find "${bundle}" -maxdepth 1 -name 'libggml-cuda.so*' -type f -print -quit)"
cuda_needed="$(needed "${cuda_backend}")"
for library in libcudart.so libcublas.so libcuda.so; do
  require_needed "${cuda_needed}" "${library}"
done

provider_needed="$(needed "${bundle}/libonnxruntime_providers_cuda.so")"
if ! grep -Eq '^lib(cuda|cudart|cublas|cudnn)' <<<"${provider_needed}"; then
  echo "ONNX Runtime CUDA provider has no CUDA runtime dependency" >&2
  exit 1
fi

echo "CUDA bundle linkage policy passed for ${bundle}"
echo "zg dependencies:"
echo "${binary_needed}"
echo "llama.cpp CUDA backend dependencies:"
echo "${cuda_needed}"
