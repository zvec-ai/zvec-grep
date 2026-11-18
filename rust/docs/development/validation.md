# Runtime and release validation

Run local commands from the workspace directory, currently `rust/`.
Start with the ordinary checks in [contributing](../../CONTRIBUTING.md#verification).
This page owns CI coverage and optional real-runtime checks;
[releasing](releasing.md) owns artifact preparation and publication commands.

## Workflow entry points

Workflow paths are relative to the repository root.

| Workflow | Purpose | Dispatch controls |
| --- | --- | --- |
| [rust-ci.yml](../../../.github/workflows/rust-ci.yml) | Workspace tests and real-model verification on Linux, macOS and Windows | `cuda_verify`, `platform=all\|linux\|windows`, optional `ref` |
| [rust-publish.yml](../../../.github/workflows/rust-publish.yml) | Native packages, standalone bundles, installed-runtime validation and publication | `publish`, `cuda`, `platform`, `dist_tag` |
| [rust-package-build.yml](../../../.github/workflows/rust-package-build.yml) | Reusable platform build and packaging job | Called by the release workflow |

Package validation compiles on CPU runners, including CUDA-capable packages.
GPU jobs execute verified artifacts without rebuilding or repacking.
`publish=false,cuda=false` produces candidates without GPUs or publication;
`cuda=true` adds GPU validation. Publication always requires GPU validation.

## Workspace and real-model CI

Push/PR runs use ordinary Linux, macOS
and Windows runners. The independent non-CUDA quality job runs formatting,
per-package checks, Clippy and npm packaging tests. Each test job compiles the
workspace tests once, runs them with pinned cargo-nextest 0.9.148 and checks the
CLI. It reuses the compiled `zg-engine` test binary for all four real models
without a separate Cargo build per model.

For manual `workflow_dispatch`, set `cuda_verify=true` to build CUDA-enabled
tests on ordinary CPU runners and add separate validation jobs on
`gpu-linux-t4` and `gpu-windows-t4` in `gpu-runners`.
`platform` accepts `all` (the default), `linux`, or `windows`. Optional `ref`
selects the reviewed source; otherwise the workflow's selected commit is used.
The source job resolves it once to a SHA for all subsequent checkouts.
CPU build/test jobs enable `zg/cuda` from the first Rust compilation and require
the pinned multi-architecture SDK. They run workspace/CPU model tests and upload
`rust-cuda-tests-<platform>`: the two selected test executables, runtime libraries
and a source-SHA/file-hash manifest, not the compiler or build cache. GPU jobs
verify and execute that relocated bundle on T4 (SM 75), without Cargo or native
compilation.
The quality job remains non-CUDA, and macOS stays non-CUDA when `platform=all`.
This workflow does not call `rust-package-build.yml` or build/install npm packages;
it is not a substitute for package-level GPU validation.

The CPU model stage runs the existing ignored real-model test for Model2Vec
(`local/potion-code-16m-v2`), Transformers/ORT (`local/all-minilm-l6-v2`), native
Bekko (`local/bekko-256-int8`) and llama.cpp (`local/embeddinggemma-300m`).
The Bekko smoke also compares every output component with its pinned ONNX
reference, including truncation boundaries and concurrent batches.
Separate GPU jobs run ORT and GGUF
with `ZVEC_GREP_SMOKE_DEVICE=cuda`, sequentially within each platform job;
Model2Vec and Bekko remain CPU-only. Linux uses separate CUDA 12/cuDNN and CUDA 13 runtime
containers for these GPU cases, with the same compiled test executable. Windows
installs both runtime families before executing the downloaded test binaries.
The test downloads, loads and embeds with each model and checks vector dimensions
and finite/nonzero output. GPU jobs reuse the CLI validator's evidence assertions to require CUDA
graph placement or positive GGUF layer offload without CPU fallback. Logs are
retained as `real-model-evidence-<platform>-<cuda|default>` for CPU build/test
jobs and `real-model-evidence-<platform>-gpu` for GPU jobs, including on failure.
Any failed model case or CUDA evidence assertion fails the platform job.

Both GPU jobs also run `cached_minilm_cuda_sessions_exit_cleanly` in fresh
subprocesses. It checks failed CUDA model loads, recovery/reload, cancellation,
and two actual CUDA sessions per pool, including the final process exit status.
Linux repeats with heap perturbation and disabled tcache, and runs the parent
and its children under Valgrind Memcheck with an error exit code. This catches
invalid accesses during ORT/provider shutdown even when inference passes.

Both GPU jobs then explicitly run the ignored C5 tests from those same compiled
test binaries, each in a fresh process: `cached_embeddinggemma_uses_cuda` verifies
CPU inference followed by CUDA inference in one process, while
`real_cpu_plugin_ignores_gpu_override` verifies that a real CPU plugin remains
the only registered backend even with `GGML_BACKEND_PATH` pointing at a present
CUDA plugin. They reuse the model cache and the relocated bundle's `lib/` directory;
no extra build is performed. Each selection must report exactly one passing
test, and `c5-*.log` files are retained with the other real-model evidence.
Packaged Windows CPU GGUF index/query checks also exercise the CLI's delayed
DLL imports, which ordinary Rust test executables do not cover.

## Live model downloads

The two live-source download suites are ignored by ordinary `cargo test` runs.
Run both explicitly from the workspace directory:

```sh
cargo test --locked -p zg-engine --lib models::artifacts::downloader::tests::live_sources:: -- --ignored --nocapture --test-threads=1
```

Each suite dynamically enumerates every local catalog model and fully downloads
its pinned artifacts when configured for the selected source (Hugging Face or
ModelScope), using a fresh temporary cache for every model/source pair and
verifying artifact integrity. Remote API models are excluded.
The suites require live network access, never fall back to the other source, and
do not run inference. They emit a catalog manifest and a JSON result for every
configured model/source pair. The normal Linux CPU CI job runs both suites using
its existing test binary, allows up to one hour, and requires passing results for
every configured model/source pair in both catalog manifests; CUDA and
Windows-only manual runs skip this gate.

To recheck one model from one source after a failure, set
`ZVEC_GREP_TEST_DOWNLOAD_MODEL` to its local catalog reference and select the exact
source suite, for example:

```sh
ZVEC_GREP_TEST_DOWNLOAD_MODEL=local/potion-multilingual-128m cargo test --locked -p zg-engine --lib models::artifacts::downloader::tests::live_sources::downloads_all_local_models_from_hugging_face -- --ignored --exact --nocapture
```

An unknown model reference fails the test. Without this variable, both suites
cover the full local catalog. Filtered runs still emit the complete catalog
manifest, so CI rejects partial coverage and continues to require every
model/source pair.

## Package validation

Collection records the exact native/component/meta tarball hashes. Independent
CPU and requested GPU jobs install those bytes; no job repairs or repacks them.
Partial platform candidates are validation-only and cannot be published.

CPU artifact checks run CLI and daemon IPC on every selected target. Default
`auto` installation must make zero CUDA metadata/tarball requests, install no
CUDA plugins and leave the plugin cache empty. A loopback registry records
requests; optional download failure followed by CPU fallback is not a pass.
Linux x64/ARM64 run in clean AlmaLinux 8.10 (glibc 2.28), load the installed
ORT 1.21.0 core and verify API 21. Linux/Windows x64 also run real GGUF/ONNX
index/query checks with explicit CPU and automatic CPU fallback; Linux ARM64
uses explicit CPU. macOS and Windows ARM64 retain CLI/daemon-only artifact smoke.

GPU jobs install a CPU base, run CPU inference, then add exact-version CUDA
components through `ZG_INSTALL_ACCELERATOR=cuda npm rebuild` in the same
prefix. The core identity and executable must remain unchanged. Both this
upgrade and fresh automatic GPU installation require verified plugins and real
inference evidence. GGUF must report positive layer offload; ORT must place
nodes on `CUDAExecutionProvider`. Model-level CPU fallback fails the check;
ORT may still execute individual supported operators on CPU.

Linux tests CUDA 12/cuDNN and CUDA 13 in separate runtime containers; Windows
tests both runtime families and their coexistence. Evidence must identify the
selected GGUF primary/fallback plugin. Installation receipts, request logs,
per-command output and GPU/driver details are retained, including on failure.
CPU/GPU checks remain independent; a failure blocks publication while preserving
the candidate and other validation results.

After publication, all six native platforms install from the registry through
npm's actual Unix symlink or Windows PowerShell entry. Fresh and upgraded
versions must pass Potion, GGUF and ONNX CPU indexing, vector/FTS/hybrid search,
incremental add/delete, daemon IPC and MCP stdio search, followed by rollback
to a distinct published version. Default CPU installs must not download CUDA.
A separate explicit plugin-fetch check verifies component versions, hashes and
the reconstructed runtime. All six jobs must pass before `latest` promotion.

Standalone smoke tests exercise install, update, failure recovery and uninstall
on all six CPU platforms with Node/npm absent from the installer's PATH. See
[standalone release workflow](../../standalone/README.md#release-workflow).

## Windows GPU runner preparation

Both GPU workflows use `scripts/cuda/prepare-windows.ps1` on the ephemeral Windows
runner. It upgrades older NVIDIA drivers to the pinned Data Center 591.59 driver,
authenticates the installer signature, and verifies the active driver version,
CUDA Driver API initialization and device enumeration. A reboot requirement, an
inactive upgrade or failed CUDA initialization fails the job explicitly; the
helper never reboots the runner.

Rust CI and package GPU jobs both call the helper with `-DriverOnly`; they do
not provision Visual Studio, Rust, nextest or portable PowerShell on the GPU
runner. Rust compilation, linking and nextest run on ordinary CPU runners
using the pinned SDK. GPU jobs install only the driver/runtime prerequisites
and execute the already-built test binaries or installed CLI. Rust CUDA CI
transfers the CUDA 13 GGUF plugin and ORT CUDA 12 provider; package CI also
validates the CUDA 12.4 GGUF fallback. This provisioning is not part of npm
installation.
Both paths configure CUDA DLL lookup, including CUDA 13's `bin/x64` as well as
`bin`, and independently load the selected GGUF CUDA plugin and its dependencies
before real inference. DLL-load evidence is saved in `windows-cuda-load.json`
inside the corresponding model/package evidence artifact. Windows package E2E
uses a short `RUNNER_TEMP` workspace; it does not alter zg/zvec long-path handling.
Linux GPU runners must supply a compatible driver and NVIDIA container support.
CPU fallback after a driver/runtime failure does not count as successful GPU
validation.

## ONNX Runtime checks

Runtime pins, cache controls and overrides are documented in
[native builds](building.md#onnx-runtime-distributions).

```sh
python3 scripts/test-prepare-onnxruntime.py
python3 scripts/test-check-glibc.py
```

Linux release jobs also execute the real engine build script in a
network-disabled builder with Vulkan enabled, Cargo offline and a verified CPU
ORT cache. This verifies offline runtime preparation, not a complete Vulkan build.

Local full-catalog/performance validation has no workflow entry point. On Linux
x64/ARM64, first collect Cargo JSON messages:

```sh
cargo test --release --locked -p zg-engine --lib --no-run --message-format=json > cargo-messages.json
```

Parse the `compiler-artifact` record whose target name is `zg_engine` and
`profile.test` is `true`; use its non-null `executable` as `--test-binary`:

```sh
python3 scripts/validate-onnxruntime-models.py \
  --test-binary /absolute/path/to/zg_engine-test \
  --cargo-messages cargo-messages.json --target x86_64-unknown-linux-gnu \
  --model-cache /absolute/path/to/models --output /absolute/path/to/results
```

Use `aarch64-unknown-linux-gnu` for a native Linux ARM64 build. The script checks
all six catalog ONNX graphs and compares official ORT 1.21/1.28 q4 CPU throughput;
`--catalog-only` skips the comparison and `--cache-dir` overrides the runtime
archive cache. The comparison uses one release binary, fixed CPU/batch/concurrency
settings, warmup and alternating runtime order. It measures the native runtime
change, not different Rust bindings. Logs, raw rounds and machine details are
written under `--output`, not CI job artifacts.
