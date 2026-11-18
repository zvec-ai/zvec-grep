# Native builds and local installation

Run commands from the workspace directory, currently `rust/`. The
[contributor guide](../../CONTRIBUTING.md) covers development checks;
[validation](validation.md) covers real models and platform gates.

## Prerequisites

- Use the toolchain pinned in [rust-toolchain.toml](../../rust-toolchain.toml).
- Install the platform's Rust linker/build prerequisites, Python 3 and curl.
  `PYTHON` can select the Python interpreter.
- Node.js 20+ and npm are needed for this workspace's packaging scripts.
  Running the native CLI does not require Node.js.
- Source builds of native dependencies need their C/C++ toolchains. Matching
  prebuilt SDKs avoid native compilation but still compile the Rust application.

Lexical search embeds the `grep` and `ignore` crates; a system `rg` executable
is not required.

## Cargo builds

```sh
cargo build --release -p zg
cargo run -p zg -- --rg needle .
```

Plain Cargo builds are non-CUDA unless `--features cuda` is selected. Cargo
stages the required native libraries beside executables, tests and examples.
Shared llama/ggml builds also stage the linked core libraries in Cargo's
`deps` and `examples` directories. Optional backends use the selected SDK or
source cache. This development output is not a portable distribution; use
packaging to assemble a complete runtime.

## Native dependencies

[Cargo.toml](../../Cargo.toml) and [Cargo.lock](../../Cargo.lock) define the
binding versions and pinned llama revision. Build and package scripts select
the native payloads:

| Dependency | Build and deployment |
| --- | --- |
| zvec | `zvec-rust` and `zvec-rust-build` select and stage the native library. The engine uses the embedded Jieba dictionary. |
| llama.cpp / ggml | The pinned fork selects a verified SDK for matching targets, features and native options. Custom configurations use source builds in `auto` mode. Download or integrity failures fail the build. |
| ONNX Runtime | The engine build script stages official ORT 1.21.0 archives pinned in [onnxruntime-distributions.json](../../scripts/onnxruntime-distributions.json). The Rust binding uses API 21 and dynamic loading. |

llama build controls:

- `LLAMA_CPP_BUILD_MODE=prebuilt` requires a matching SDK; `source` rebuilds.
- `LLAMA_CPP_RUNTIME_CACHE` selects the SDK cache, defaulting to
  `$CARGO_HOME/llama-cpp-runtimes`; `LLAMA_CPP_OFFLINE=1` requires it populated.
- Source archives use `$CARGO_HOME/llama-cpp-sources`, overridden by
  `LLAMA_CPP_SOURCE_CACHE`. `LLAMA_CPP_SOURCE_DIR` selects an explicit source tree.

These are build controls, independent of installation-time npm plugins.

## ONNX Runtime distributions

The build script runs [prepare-onnxruntime.py](../../scripts/prepare-onnxruntime.py),
including for standalone engine tests. CPU builds select CPU/CoreML/DirectML
runtimes; x64 CUDA builds select the CUDA 12 core and providers. macOS model
routing uses CPU.

`ZG_ORT_CACHE_DIR` overrides the archive cache; otherwise it is
`target/ort-prebuilt/` (under the target triple for cross builds).
`CARGO_NET_OFFLINE=true` requires verified cached archives.
`ORT_DYLIB_PATH` supplies an external runtime supporting API 21 and skips
automatic preparation. ONNX WebGPU requires a runtime containing WebGPU through
that override; enabling GGUF Vulkan alone does not.

On Windows the engine preloads sibling DirectML before ORT initialization.
ORT loads CUDA providers from its own runtime directory; NVIDIA drivers,
CUDA 12 libraries and cuDNN 9 remain host dependencies. See
[validation](validation.md#onnx-runtime-checks) for deployment and catalog checks.

## CUDA builds

The npm build/pack/install commands prefer CUDA on Linux/Windows x64. In
default `auto` mode, no `nvcc` on PATH produces a CPU-only package.
`--cpu-only` selects CPU explicitly. Explicit prebuilt mode selects the CUDA
SDK on x64 without a CUDA compiler:

```sh
npm run build
npm run build -- --cpu-only
# CUDA-capable x64 package without nvcc (PowerShell: set $env:LLAMA_CPP_BUILD_MODE):
LLAMA_CPP_BUILD_MODE=prebuilt npm run build
```

CUDA **source** builds require Toolkit 13.1 on PATH and Toolkit 12.4 at `CUDA12_ROOT`
(default `/usr/local/cuda-12.4` on Linux, or `CUDA_PATH_V12_4` on Windows).
An installed but unsupported primary toolkit, missing fallback toolkit, or
failed CUDA compilation is an error, not a silent CPU fallback. Building does
not require a GPU. Both variants need the normal Rust/C++ build prerequisites;
CUDA additionally uses CMake, Ninja, Python 3 and libclang, plus patchelf/readelf
on Linux or Visual Studio 2022 Build Tools on Windows.

The direct CUDA entry points are `scripts/cuda/build.sh` and
`scripts/cuda/build.ps1`. They create `target/release/zg-cuda/{bin,lib}` by
default; the npm entry point isolates them under `target/cuda/` and CPU builds
under `target/cpu/`. `CARGO_TARGET_DIR` changes the parent output directory.
Source-mode CUDA 12.4 on Windows requires MSVC 14.38 before running
`scripts/cuda/build-fallback.py`. Matching SDK builds copy that plugin directly,
so neither a second CMake build nor the fallback toolchain is needed.

Release CI requires matching prebuilt SDKs on all six targets. Runtime staging
uses the current Cargo invocation's backend directory. Linux release binaries
and bundled libraries are audited against glibc 2.28.
See [npm distribution](../../npm/README.md#platforms-and-native-runtimes) for the
platform matrix, primary/fallback CUDA architecture lists and driver checks.

## Local npm installation

Build the release binary:

```sh
npm run build
```

Stage only the current native platform and link the local package into the
active npm global prefix:

```sh
npm run install:local
zg --version
```

The local install remains linked to `dist/npm/zvec-grep-local`; rerun the command
after cleaning `dist/`. To generate a self-contained tarball without installing
it:

```sh
npm run pack:local
```

Generated packages are written under `dist/npm/`. Run the isolated install
smoke test with:

```sh
npm run test:package
```

Pass `--no-build` after `--` to reuse the last successful npm build (or a plain
Cargo `target/release/zg` when no build record exists), or pass
`--prefix <path>` to install or smoke-test under a custom npm prefix:

```sh
npm run install:local -- --no-build --prefix "$HOME/tmp/zvec-grep-npm"
```

On Linux and Windows, manual CPU packaging rejects runtime libraries outside
the known CPU set (zvec and CPU/DirectML ORT). A CUDA-to-CPU Cargo rebuild can
leave old shared libraries beside `zg`; it does not clean that directory.
If packaging rejects such a mixed directory, rebuild with `--cpu-only` in a
fresh `CARGO_TARGET_DIR`, or use an audited CUDA bundle. Packaging never deletes
the original libraries or silently includes them in a CPU package.

For registry artifacts and publication, continue with [releasing](releasing.md).
For installation without npm, use the [standalone installers](../../standalone/README.md).
