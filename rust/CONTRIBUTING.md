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
assemble the primary bundle, `build-fallback.py` adds only the CUDA 12.4 plugin,
`prepare-onnxruntime.py` verifies and stages official ORT archives, and
`check.sh`/`check.ps1` audit the resulting runtime linkage. `prepare-windows.ps1`
and the two pinned requirements files provision Windows build/runtime tools.
`cuda-builder.Dockerfile` provides the shared Linux CUDA build environment for
source CI and package builds.
`e2e.py` validates installed packages on a real GPU. CUDA validation consists
of actual compilation, linkage audits and GPU end-to-end execution; ordinary
PR CI retains the npm packaging tests without a separate CUDA mock-test suite.

`Rust CUDA CI` (`.github/workflows/rust-cuda-ci.yml`) only runs through
`workflow_dispatch`. Select a reviewed source ref and check `confirm_reviewed`.
The workflow resolves the ref once to a SHA, then runs formatting, CUDA-enabled
workspace checks, Clippy, test compilation, workspace tests, a CLI smoke test and
both real model tests in one job per platform on `gpu-linux-t4` and
`gpu-windows-t4` in `gpu-runners`.
It targets T4 (SM 75), does not call `rust-package-build.yml`, and does not build
or install npm packages. It is not a substitute for package-level GPU validation.

The model stage reuses the `zg-engine` test executable from the workspace build,
without invoking Cargo or compiling again for either model. It runs the existing
ignored real-model test with `ZVEC_GREP_SMOKE_DEVICE=cuda` for Transformers/ORT
(`local/all-minilm-l6-v2`) and llama.cpp (`local/embeddinggemma-300m`) on Linux and
Windows x64, sequentially within each platform job. Linux uses separate CUDA 12/
cuDNN and CUDA 13 runtime containers for the same compiled test executable;
Windows installs both runtimes before building. Model2Vec stays in `Rust Models CI`
because it has no CUDA backend.
The test downloads, loads and embeds with each model, checks vector dimensions
and finite/nonzero output, and reuses the CLI validator to require CUDA graph
placement or positive GGUF layer offload without CPU fallback. Logs are retained
as `cuda-model-evidence-*`, including on failure. Either model failing fails the
platform job; the original CPU model test coverage is unchanged.

`Publish Rust packages` handles package-level validation. Pushes to `main` and
`release/rust-*`, and manual runs with both `publish=false` and `cuda=false`,
build/audit/package on ordinary CPU runners without allocating GPUs or publishing
to npm. Set `cuda=true` with `publish=false` to validate GPU packaging without
publishing. Either `cuda=true` or `publish=true` switches the Linux/Windows x64
jobs to GPU runners and enables `.github/actions/cuda-cli-e2e`; publication cannot
bypass GPU validation by leaving `cuda` unchecked. These jobs compile, audit, pack, install and
exercise their own CUDA npm packages with the full release architecture list.
CPU runner selection does not change the default CUDA-capable package variant.

After collection, separate Linux/Windows x64 CPU jobs download the same release
artifact, verify tarball SHA256 values, and install native plus meta tarballs
offline into a fresh prefix without rebuilding or repacking. They verify CLI and
daemon IPC, then run `scripts/cuda/e2e.py --cpu-only` against the installed meta
package. This checks both explicit `--device cpu` and default automatic CPU
fallback for real GGUF/ONNX indexing and vector queries. Linux uses a plain
Ubuntu 24.04 container; Windows excludes toolkit paths. Both require CUDA host
libraries to be unavailable and retain `cpu-artifact-evidence-*` logs on failure.
CPU artifact validation runs even when `publish=false`; failure blocks npm
publication, while the downloaded candidate remains available for diagnosis.

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
caches are reused, and concurrent runs of the same CUDA CI workflow are queued.
Ordinary `Rust CI` runs non-CUDA builds/tests. Release builds can compile CUDA
packages on CPU runners. Package-level GPU execution requires `cuda=true` or
`publish=true`; npm publication still requires an explicit `publish=true` run.

To reproduce against an unpacked/installed CUDA bundle (Python 3.12 recommended):

```sh
python scripts/cuda/e2e.py --bundle /path/to/package --model-cache /path/to/models --logs /path/to/logs --expected-gguf-cuda 13.1
```

Both platforms first run GGUF and ONNX with only CUDA 12/cuDNN 9 visible,
requiring GGUF to select the CUDA 12.4 fallback. Linux then runs GGUF in a separate
CUDA 13.1 container. Windows hides the compiler toolkit's CUDA 13 DLL directory
from PATH for the fallback test, then restores CUDA 13.1 and reruns both models,
requiring the GGUF primary plugin and ONNX CUDA 12 provider to coexist. Logs must
prove the expected GGUF plugin as well as actual GPU/provider selection.
The Windows job provisions VS 2022 C++ tools (including MSVC 14.38), a pinned
libclang wheel and the repository-pinned Rust toolchain on the Data Science VM.
The job only inspects the runner's existing NVIDIA driver; CI never downloads,
upgrades or reboots a driver. Fix an incompatible runner image outside the job.
The Linux image must supply a compatible driver and NVIDIA container support.
Driver/runtime failures never count as successful CPU execution.
