# Rust rewrite contributor guide

The Rust workspace lives in `rust/`. Run the commands in this guide from that
directory; the repository root contains the TypeScript / Node.js implementation.

## Start

From the repository root:

```sh
cd rust
bash scripts/check.sh
cargo run -p zg -- --rg needle .
```

The workspace pins its Rust toolchain in `rust-toolchain.toml`. Lexical search
uses embedded `grep` and `ignore` crates, so a system `rg` executable is not
required for builds or tests.

## Engine changes

Keep the application surface centered on `ZvecGrep`:

- add a typed method or extend its request/reply types;
- implement behavior in a private service module;
- call that service directly from `ZvecGrep`;
- return `EngineError` without wrapping the result in a generic command outcome.

Do not add a generic `Core`, command bus, operation envelope, adapter registry or
transport executor to connect an in-process method to its implementation.

`zg_engine::authorization` is an explicit exception for remote-consent preflight
and signed grant-file management. CLI, daemon and MCP callers must be able to
resolve destinations and inspect, grant or revoke consent before starting an
engine operation. These functions use workspace metadata and authorization files;
they do not require a `ZvecGrep` instance, acquire model runtimes, open index
storage or send remote requests. Keep their implementation helpers private and
return typed data for new callers; terminal rendering belongs in `zg-cli`.
The existing string-returning helpers remain for compatibility. This exception
does not extend to indexing, search or other engine operations, which continue
to use typed `ZvecGrep` methods backed by private services.

## Native and transport changes

Native dependency types remain in their owning crate. Daemon framing and wire
commands remain in `zg-daemon-protocol`; MCP schemas remain in
`zg-transport-mcp`. Transport handlers call public `ZvecGrep` methods directly.

If a private engine service needs code currently living in another crate,
prefer moving that concrete implementation behind the engine boundary. Do not
make an engine-internal module public solely to avoid a crate dependency cycle.

## Compatibility

The TypeScript implementation in the repository root is the behavioral oracle
during the rewrite. Store stable, machine-readable cases under `compat/` and normalize
paths, random identifiers and timings in the runner.

## Verification

Run:

```sh
cargo fmt --all --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc -p zg-engine --no-deps
```

## CUDA validation

CUDA-specific helpers live only in `scripts/cuda/`: `build.sh`/`build.ps1`
assemble the bundle; `runtime.py` stages the exact native runtime selected by
Cargo, including the SDK's CUDA 12 fallback. In explicit source mode,
`build-fallback.py` builds only the CUDA 12.4 plugin, using `--cargo-messages`
from the successful primary build to locate that build's verified source
directory and revision. This supports the fork's source cache and
`LLAMA_CPP_SOURCE_DIR` override without assuming a submodule directory or
selecting a stale build tree. Prebuilt bundles copy the SDK's fallback instead.
`prepare-onnxruntime.py` verifies and stages official ORT archives, and
`check.sh`/`check.ps1` audit the resulting runtime linkage. `prepare-windows.ps1`
with `-DriverOnly` and the pinned runtime requirements prepare Windows GPU
drivers/runtime libraries; compiler tools stay on CPU runners.
`cuda-builder.Dockerfile` provides Linux Rust build dependencies; CI selects a
base image without a CUDA compiler for packaging and uses separate CUDA runtime
images for GPU tests.
`e2e.py` validates installed packages on a real GPU or, with `--cpu-only`, on a
host without loadable CUDA libraries. CUDA validation consists of actual
Rust compilation/linking against the prebuilt SDK, linkage audits and GPU
end-to-end execution. The fork's SDK release gates own llama.cpp/CUDA compilation;
ordinary PR CI retains
the npm packaging tests without a separate CUDA mock-test suite.

### Workspace and real-model CI

`Rust CI` (`.github/workflows/rust-ci.yml`) is the single entry point for
workspace and real-model verification. Push/PR runs use ordinary Linux, macOS
and Windows runners. The independent non-CUDA quality job runs formatting,
per-package checks, Clippy and npm packaging tests. Each test job compiles the
workspace tests once, runs them with pinned cargo-nextest 0.9.148 and checks the
CLI. It reuses the compiled `zg-engine` test binary for all three real models
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
(`local/potion-code-16m-v2`), Transformers/ORT (`local/all-minilm-l6-v2`) and
llama.cpp (`local/embeddinggemma-300m`). Separate GPU jobs run ORT and GGUF
with `ZVEC_GREP_SMOKE_DEVICE=cuda`, sequentially within each platform job;
Model2Vec remains CPU-only. Linux uses separate CUDA 12/cuDNN and CUDA 13 runtime
containers for these GPU cases, with the same compiled test executable. Windows
installs both runtime families before executing the downloaded test binaries.
The test downloads, loads and embeds with each model and checks vector dimensions
and finite/nonzero output. GPU jobs reuse the CLI validator's evidence assertions to require CUDA
graph placement or positive GGUF layer offload without CPU fallback. Logs are
retained as `real-model-evidence-<platform>-<cuda|default>` for CPU build/test
jobs and `real-model-evidence-<platform>-gpu` for GPU jobs, including on failure.
Any failed model case or CUDA evidence assertion fails the platform job.

Both GPU jobs then explicitly run the ignored C5 tests from those same compiled
test binaries, each in a fresh process: `cached_embeddinggemma_uses_cuda` verifies
CPU inference followed by CUDA inference in one process, while
`real_cpu_plugin_ignores_gpu_override` verifies that a real CPU plugin remains
the only registered backend even with `GGML_BACKEND_PATH` pointing at a present
CUDA plugin. They reuse the model cache and the relocated bundle's `lib/` directory;
no extra build is performed. Each selection must report exactly one passing
test, and `c5-*.log` files are retained with the other real-model evidence.
The packaged Windows CLI also exercises delayed DLL imports, unlike ordinary
Rust test executables: CPU variant scoring must first resolve the ggml core
import through its device-independent timer initialization, so releasing a
temporary CPU plugin cannot unload the core and leave its exception handler
dangling. The existing packaged CPU GGUF index/query checks cover this path.

### Package validation

`Publish Rust packages` (`.github/workflows/rust-publish.yml`) handles
package-level validation. It runs only through manual dispatch. Runs with both
`publish=false` and `cuda=false` build/audit/package on ordinary CPU runners
without allocating GPUs or publishing
to npm. Compilation always stays on CPU runners, including when `cuda=true` or
`publish=true`; Linux/Windows x64 still build the default CUDA-capable bundles.
Set `cuda=true,publish=false` to add GPU validation without publishing.
Separate GPU jobs download the exact native npm artifacts from the same run and
invoke `.github/actions/cuda-cli-e2e`, without recompiling native code or repacking. Publication
always requires these GPU checks, even when `cuda` is unchecked. Collection runs
first; CPU and requested GPU validation then run independently in parallel.
Failures retain candidates and do not skip the other validation path, but every
required check must pass before publication. Collection alone is not approval.

The manual `platform` input selects `all` (the default) or one of `darwin-arm64`,
`darwin-x64`, `linux-x64-gnu`, `linux-arm64-gnu`, and `win32-x64-msvc`.
Selected platforms compile in parallel; Intel macOS does not gate the others.
All-platform runs collect `rust-release-<version>`; a single-platform run collects
`rust-candidate-<version>` with `partial: true` in `release.json`. The latter is
validation-only and cannot be published or promoted; `publish=true` requires
`platform=all`. Each collected artifact includes the selected native tarballs,
the exact-version meta tarball, checksums and installation instructions.

After collection, separate CPU jobs for every selected target download their
native artifact plus the collected meta/inventory artifact, verify tarball SHA256
values, and install base/meta tarballs into a fresh prefix without compiling or
repacking. `ZG_INSTALL_ACCELERATOR` is unset to exercise the default `auto`
behavior on the real CPU host. An isolated loopback registry serves only this
run's CUDA archives: any CUDA metadata/tarball request (even a failed optional
attempt), installed plugin or populated plugin cache fails CPU validation.
`cpu-downloads.json` retains the request evidence; no public registry is used.
They verify CLI and daemon IPC. Linux/Windows x64 then
run `scripts/cuda/e2e.py --cpu-only` against the installed meta
package. This checks both explicit `--device cpu` and default automatic CPU
fallback for real GGUF/ONNX indexing and vector queries. Linux uses a plain
Ubuntu 24.04 container; Windows excludes toolkit paths. Both require CUDA host
libraries to be unavailable and retain `cpu-artifact-evidence-*` logs on failure.
CPU artifact validation runs even when `publish=false`; failure blocks npm
publication, while the downloaded candidate remains available for diagnosis.

CPU and GPU jobs remain independent. On each GPU host, the actual npm
postinstall first installs a CPU-only base with `ZG_INSTALL_ACCELERATOR=cpu`,
then runs explicit CPU inference. In that same prefix,
`ZG_INSTALL_ACCELERATOR=cuda npm rebuild` fetches the exact unpublished component
archives through a loopback registry and must activate every verified plugin.
It must preserve the installed core identity and executable. Both CUDA 12
GGUF/ONNX inference and CUDA 13 GGUF inference use that rebuilt installation;
Windows also checks ONNX with CUDA 13 present. Linux shares the prefix between
its two runtime containers. Fresh automatic GPU installation remains separately
covered. Before/after installation receipts and `rebuild-downloads.json` are
retained with the GPU evidence, including on failure. This chain runs for
`publish=false,cuda=true`, without compiling or publishing anything. Public
registry smoke remains a separate post-publication check.

Both platforms run the same CLI index, readiness and vector-search test with
GGUF `local/embeddinggemma-300m` and ONNX `local/all-minilm-l6-v2`, explicitly
requesting CUDA with indexing concurrency 2. A successful command alone is not
a pass: index and search must select CUDA, GGUF must report positive layer offload
to the GPU, ONNX must place a nonzero number of
graph nodes on `CUDAExecutionProvider`, and any model-level CPU fallback fails
the test. ORT may still put supported shape/other nodes on CPU; this test does
not claim every ONNX operator runs on GPU.

Per-command stdout/stderr, GPU/driver details and a machine-readable result are
retained as `cuda-cli-evidence-*` artifacts, including when a model fails. Model
caches are reused. Package runs share a concurrency group only for the same
branch and platform selection; different platform selections can run concurrently.
An in-progress run is not cancelled by a newer run in its group. npm publication
still requires an explicit `publish=true` run and the release gates documented
in [npm distribution](npm/README.md).

To reproduce against an unpacked/installed CUDA bundle (Python 3.12 recommended):

```sh
python scripts/cuda/e2e.py --bundle /path/to/package --model-cache /path/to/models --logs /path/to/logs --expected-gguf-cuda 13.1
```

Package GPU jobs first run GGUF and ONNX with CUDA 12.8.1/cuDNN 9 visible,
requiring GGUF to select the plugin built with CUDA 12.4. Linux then runs GGUF in
a separate CUDA 13.1 runtime container. Windows hides any CUDA 13 cuBLAS DLL
directories from PATH for the fallback test, then installs CUDA 13.1 runtime
libraries and reruns both models, requiring the GGUF primary plugin and ONNX
CUDA 12 provider to coexist. Logs must prove the expected GGUF plugin and actual
GPU/provider selection. See [Native and CUDA builds](README.md#native-and-cuda-builds)
for the distinct primary and fallback distribution architecture lists.

### Windows GPU runner preparation

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
