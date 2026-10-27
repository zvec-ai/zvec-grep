# npm distribution

The release layout has one small meta package, one base native package for each
published target, and exact-version CUDA component packages for Linux/Windows
x64. `platforms.json` is the single source of truth for package
names and npm `os`, `cpu`, and `libc` selectors.

The current published targets are macOS ARM64/x64, Linux x64/ARM64 with glibc,
and Windows x64/ARM64.

After installation, the meta package retains the existing runtime paths. CUDA
files are present only after successful plugin installation:

```text
bin/zg.exe              # meta entry on every OS; command remains zg
lib/libzvec_c_api.*       # storage runtime with embedded Jieba dictionary
lib/lib{llama,ggml}*      # CUDA variant: shared llama.cpp core and CPU backend
lib/*onnxruntime*        # ONNX Runtime core/providers, when dynamically linked
lib/*ggml-cuda*          # primary CUDA 13.1 GGUF plugin
lib/cuda12/*ggml-cuda*    # CUDA 12.4 GGUF fallback plugin
```

Source npm packages also carry `checksums.json` and, once reviewed inputs are
available, `LICENSES/` and `SBOM.spdx.json`; these are package metadata rather
than runtime files copied by the installer.

The Linux x64 base package's `plugins.json` references `-ggml-cuda13`, `-ggml-cuda12`,
`-ort-cuda-part1` and `-ort-cuda-part2` packages. Windows x64 references
`-ggml-cuda13`, `-ggml-cuda12` and `-ort-cuda`. These suffixes extend the base
platform package name. macOS, Linux ARM64 and deliberate CPU-only builds remain
single native packages. A default all-platform release contains 14 tarballs:
six base packages, seven runtime components and the meta package.

The base package retains the executable and CPU/core libraries, including
zvec's embedded Jieba dictionary; no external dictionary files are required.
CUDA components contain whole GGML plugins or the ORT provider. The Linux ORT
provider is split into two ordered byte ranges under `parts/`; postinstall
streams them back into the original `.so` and verifies the original complete
file's SHA256. No compiler, model execution or CUDA Toolkit installation is
performed during npm installation. Meta still selects its base by OS/CPU/libc,
but CUDA components are no longer npm dependencies. Postinstall first verifies
and installs CPU, then uses bounded NVIDIA detection to decide whether to fetch
the complete pinned CUDA set. CPU-only hosts make no CUDA component requests.
GPU installs still download the whole set; splitting limits each upload, not
their total size. Downloaded archives may occupy cache space in addition to the
assembled libraries.
Auto detection checks only whether `nvidia-smi` succeeds within five seconds
and returns a GPU. It does not compare the reported driver version, validate
CUDA/cuDNN dependencies, or select a per-backend subset of plugins. GGUF and ORT
still choose their execution backends independently at runtime.

Use `ZG_INSTALL_ACCELERATOR=cpu|auto|cuda` (default `auto`) with npm install or
rebuild. `cpu` skips detection/downloads; `cuda` explicitly requests plugins even
on a GPU-free builder and fails on any plugin error. Auto failures retain CPU
with diagnostics. A forced plugin failure also leaves the CPU core intact.
Later use `ZG_INSTALL_ACCELERATOR=cuda npm rebuild -g @zvec/zvec-grep` (omit `-g`
for project-local installs). Stop zg first and restart the daemon afterwards.
There is no extra setup command or runtime downloading.

`ZG_PLUGIN_DIR` is an authoritative absolute directory of exact-version `.tgz`
files. `ZG_INSTALL_OFFLINE=1`/npm offline forbids network requests;
`ZG_PLUGIN_CACHE` overrides the content-addressed download cache. Missing local
files never fall back to a registry version. npm handles registry, proxy, auth
and custom CA settings for online transport, with component scripts disabled.
Archives, payloads and reconstructed libraries are verified before activation;
the cache is not a permanent runtime library path. See [installation and recovery](meta/README.md).

Every compressed `.tgz` has a **350 MiB maximum**, enforced after `npm pack`,
during collection, and again before publication/promotion. This leaves room
below the approximately 384 MiB tarball ceiling caused by npm's base64/JSON
publication path and 64-bit Node's string limit. `npm pack` succeeding alone is
not publication validation. A component that outgrows the budget fails closed;
it must be split further, not silently published as a reduced CPU package.

`LICENSES/` and `SBOM.spdx.json` require reviewed per-target compliance inputs;
validation candidates may omit them, but publication and promotion may not.

`bin/` contains only the native executable. All runtime libraries, providers,
and plugins live under `lib/`. Unix binaries use a `../lib`
rpath. Windows uses a native MSVC delay-load hook to resolve DLLs from the
executable's sibling `lib/`. The binary runs directly, without a JavaScript
launcher or custom loader environment variables for bundled libraries. Host
NVIDIA libraries must still be discoverable by the system loader.

Non-CUDA builds enable ORT's upstream `copy-dylibs` feature to stage runtime
sidecars beside the Cargo executable. npm packaging copies them into `lib/`
as regular files, including any symlinked libraries. This includes Intel macOS's
dynamically linked ORT and Windows DirectML's DLL; it does not change the static
ORT linkage used by the default macOS ARM64 and Linux CPU builds. CUDA bundles
load ORT dynamically and continue to stage their pinned core/providers explicitly.

The default `linux-x64-gnu` and `win32-x64-msvc` native packages are CPU/CUDA
bundles. CUDA libraries remain runtime plugins: compatible NVIDIA libraries
enable GPU use, while hosts without them continue on CPU. Release maintainers
may use `--cpu-only` for a CPU-only build, but that variant is not
published as the default x64 package on Linux or Windows.

In default build mode, `npm run build` and release packaging prefer CUDA on those two targets when
`nvcc` from CUDA 13.1 is available. If `nvcc` is absent they build a CPU-only
package instead. An installed but unsupported CUDA toolkit is an error; use
`--cpu-only` deliberately rather than silently producing a different GPU ABI.
Matching configurations reuse the fork's pinned native SDK, including the
12.4 fallback. Only **source** builds need a second 12.4 toolkit. Explicit
`LLAMA_CPP_BUILD_MODE=prebuilt` builds the CUDA bundle without either compiler;
it fails on a configuration mismatch. An incomplete CUDA build fails and does
not silently publish a CPU package.
CPU/CUDA build trees are isolated under `target/cpu` and `target/cuda` on these
targets. `--no-build` reuses the last successful recorded build, not a stale
bundle selected by its existence. Explicit CI bundles use `--bundle-dir`.
Plain `cargo build --release -p zg` remains a non-CUDA developer build.
Manual CPU packaging (`--no-build`, including explicit `--binary`/`--lib-dir`)
rejects Linux/Windows shared libraries outside the known CPU set, rather than
packaging stale CUDA libraries left by a previous Cargo build. Use `--cpu-only`
with a fresh `CARGO_TARGET_DIR` to recover; CUDA packaging uses an audited
`--bundle-dir`. The check does not remove or change the original build outputs.

Published target/backend support is:

| Target | llama.cpp / GGUF | ONNX Runtime |
| --- | --- | --- |
| macOS ARM64 | CPU + Metal | CPU (CoreML deliberately not selected) |
| macOS x64 (14+) | CPU + Metal where available | CPU (CoreML deliberately not selected) |
| Linux x64 glibc (Ubuntu 24.04 baseline) | CPU + CUDA 13.1, CUDA 12.4 fallback | CPU + CUDA 12 |
| Linux ARM64 glibc | CPU | CPU |
| Windows x64 | CPU + CUDA 13.1, CUDA 12.4 fallback | CPU + CUDA 12 |
| Windows ARM64 | CPU | CPU + DirectML |

A deliberate CPU-only Windows build retains DirectML. The published Windows
CUDA bundle uses the official ONNX Runtime 1.28.0 CUDA 12 provider and does not
claim DirectML support. ORT requires CUDA 12 runtime/cuBLAS and cuDNN 9 even
when GGUF uses CUDA 13; the two runtime families can coexist. NVIDIA driver,
CUDA runtime, cuBLAS and cuDNN remain host dependencies, not bundled libraries.
macOS keeps its existing ONNX CPU routing because of CoreML regressions.
Linux musl, CUDA on ARM64/macOS and Vulkan packages are not in
the release matrix.

The CUDA 13.1 primary GGUF plugin uses
`75-real;80-real;86-real;89-real;90-real;90-virtual`. The CUDA 12.4 fallback uses
`50-virtual;61-virtual;70-virtual;75-virtual;80-virtual;86-real`: PTX plus native
SM86 code, reducing installed size while retaining driver JIT compilation where
compatible native code is unavailable. `CMAKE_CUDA_ARCHITECTURES` overrides the
primary source build; `CUDA12_ARCHITECTURES` overrides only the fallback, which does not
inherit the primary list. These build targets do not imply hardware validation
on every GPU generation.

Before loading either GGUF CUDA plugin, GGUF queries the active CUDA Driver API
(driver capability, not the installed Toolkit). The 13.1 primary requires at
least `cuDriverGetVersion >= 13000`. With the distribution targets, every visible
GPU must have compatible native SM75/80/86/89/90 code, or be able to use compute90
PTX with driver capability `>= 13010`. For example, SM100 requires PTX and cannot
use the primary on a driver reporting `13000`. Forced PTX also requires `13010`
and a compute90-compatible device; older GPUs cannot use that PTX even with a
new driver. A failed query, missing compatible code, or insufficient capability
skips the primary **before registration**, logs the reason, and tries CUDA 12.4.

Before loading the 12.4 fallback, its own preflight still applies.
Paths requiring the distribution's PTX 8.4 require `cuDriverGetVersion >= 12040`
(driver capability for CUDA 12.4). Older CUDA 12 drivers are allowed only when
every visible device can use the native SM86
cubin (SM86/89) and PTX JIT is not forced. A mixed set containing a PTX-only GPU
therefore cannot use the old-driver exception. Failed queries or insufficient
capability skip the fallback, log the reason, and preserve CPU execution; the
process does not rewrite `CUDA_VISIBLE_DEVICES`.
`GGML_BACKEND_PATH` overrides require `13010` before either directory loader runs,
without a native-code exception for unknown custom architectures.
Build-time custom architecture lists or an external `LLAMA_CPP_RUNTIME_DIR`
likewise disable the corresponding plugin's exception. The primary's native-code
exception requires explicit distribution targets or pinned `prebuilt` mode;
`auto`/`source` builds without explicit targets conservatively require `13010`.
Replacing plugins after a build is outside the distribution contract.
Explicit CPU execution never runs these probes; ORT selection is independent.
This preflight is not a PTX execution test or subprocess crash isolation: native
driver aborts and later inference failures are not guaranteed to recover.
CPU/GPU CI validates its specific driver/runtime environments, not every older
driver. See NVIDIA's [minor-version compatibility caveats](https://docs.nvidia.com/deploy/cuda-compatibility/minor-version-compatibility.html)
and [cubin compatibility rules](https://docs.nvidia.com/cuda/cuda-programming-guide/01-introduction/cuda-platform.html#binary-compatibility).

The primary GGUF CUDA plugin remains directly under `lib/`; there is no new
`lib/cuda13/` directory. Explicit CPU execution uses a selective CPU-plugin
loader rather than scanning that directory for every backend, and ignores
`GGML_BACKEND_PATH`. Optional GPU registration is separate from CPU initialization.
Before scoring CPU variants, a device-independent ggml timer call resolves the
core import. This keeps `ggml-base.dll` loaded for the process lifetime on
Windows, including when temporary scoring handles are released; it does not
scan GPU plugins or initialize a GPU.

On Windows, CUDA 13 runtime DLLs reside under the toolkit's `bin/x64`, while
older toolkits use `bin`. The required host runtime directories must be visible
to the loader (for example, through PATH); the package's sibling `lib/` lookup
does not install or locate missing NVIDIA runtime dependencies automatically.

The meta package locks every platform package to its exact version through
`optionalDependencies`. CUDA-capable bases use schema-3 base checksums plus a
hash-bound `plugins.json` with exact versions, core identity, archive integrity,
payload hashes and final reconstructed-library hashes. Plugin tarballs are packed
first so the base pins their actual bytes. `variant`/`accelerators` describe
compiled support; `bundledAccelerators` and `native-install.json` distinguish
the CPU base from the installed plugin set. The core remains dynamically
extensible: it is not replaced with a `--cpu-only` build. The npm command is always named `zg`; the meta
package retains the cross-platform `bin/zg.exe` native entry point so npm's
Windows shim and Unix symlink both enter the executable directly.

Self-contained native packages (macOS, Linux ARM64 and deliberate CPU-only
builds) retain schema-1 checksums. The installer supports native schemas 1 and 3;
the unpublished schema-2 format with mandatory CUDA npm dependencies is not
supported. Plugin component checksums still use their own schema 1.

The published meta package supports Node.js 14.14 and newer for installation
only, not for CLI execution. Online plugin transport requires npm 7+ and
extraction uses system `tar`; local archives need no network. The root workspace may require a newer Node version for
development and package tests without reducing published compatibility.

Release order:

1. Build and package every native target; apply ad hoc signatures on macOS.
2. Collect the exact-version meta/native/component candidates once. Verify
   payload closure and checksums, run CPU and requested GPU checks in parallel,
   and require all publication gates including licenses and SBOMs to pass.
3. Publish runtime components before their base native packages to `rust-canary`, then publish the meta package
   to `rust-canary` last. Candidate publication never changes `latest`.
4. On every platform's ordinary CPU runner, install the exact meta-package version from the
   registry alone, so npm must select its native optional dependency. These jobs
   download only the small `rust-verification-<version>` artifact for the release
   inventory, not another copy of all platforms' native/CUDA tarballs.
5. Verify CLI startup, managed-rg, daemon startup and IPC, then update from a
   distinct published baseline version and roll back to that version. On the
   CUDA targets also force a registry plugin installation and verify its hashes
   and core identity; CPU fallback must not hide a plugin-fetch failure.
6. Promote native `latest` tags and then the meta `latest` tag in a separate job,
   only after all six registry install/update/rollback jobs pass.

`.github/workflows/rust-publish.yml` builds and packs the selected targets once on
their native runners in parallel, collects the candidates into a downloadable
artifact, then checks real installation, CLI startup, managed-rg and daemon IPC.
The workflow runs only through manual dispatch and does not publish unless
`publish` is enabled. The manual
`platform` input defaults to `all`; each of the six target names can be selected
individually.
`publish=true` requires `platform=all`; a subset is a validation-only candidate,
not a complete release. Manual runs with `publish=false, cuda=false` use ordinary CPU runners;
they do not allocate GPUs or publish packages. All six package builds require
the pinned llama-cpp-rs prebuilt SDK, including the Windows ARM64 static CPU
SDK. Linux/Windows x64 still produce CUDA-capable
bundles, but neither the 13.1 primary nor 12.4 fallback is compiled in zg CI.
All packaging builds stay on ordinary CPU runners, including when `cuda=true`
or `publish=true`. Manual `cuda=true` runs add separate Linux/Windows x64 GPU
jobs that download the exact native npm artifacts from the same workflow run
and perform package-level GPU end-to-end validation, without recompiling native code or
repacking. Publishing also enables GPU validation unconditionally, even if
`cuda` was not selected. Candidate collection precedes parallel CPU/GPU checks;
it proves package completeness, not runtime success. A GPU failure does not skip
CPU checks or remove the candidate, but blocks npm publication.
Runs use branch-plus-platform concurrency groups, allowing different platform
selections on the same branch to run concurrently without cancelling an
in-progress run in the same group.
Intel macOS no longer gates the other platform builds. Its ONNX Runtime with
CoreML and zvec SDK are built from
pinned sources, cached, and bundled with the native package. The Intel release
targets macOS 14 and later.

Windows ARM64 uses `windows-11-arm` with ARM64 Node, Rust, and MSVC. It uses
zvec-rust 0.8.0's automatically selected Windows ARM64 prebuilt SDK instead of
building zvec from source. The shared DLL is staged by zvec-rust-build and
bundled in `@zvec/zvec-grep-win32-arm64-msvc`. ONNX Runtime uses the locked
ARM64 DirectML distribution with SHA-256 verification and a separate cache.
llama-cpp-rs selects the SHA-256-pinned Windows ARM64 static CPU SDK from
`zvec-0.1.154-cpu-r2`, without running bindgen, CMake, or a C++ compiler.
No Clang/Ninja overrides are needed during packaging. The installed artifacts
receive the same CLI, managed-rg, and direct/server daemon IPC checks as the
other non-CUDA targets.

Build jobs select Cargo/CMake parallelism from the runner's available CPUs and
total memory, budgeting 3 GiB per job and capping concurrency at 8. The Linux
builder receives the same limits. This is a conservative memory heuristic,
not an OOM guarantee. The pinned SDK contains the shared core, CPU variants and
both CUDA plugins; no Windows CUDA compiler/MSVC 14.38 fallback setup or second
Linux CUDA builder is needed. Rust linking and dependency audits remain. The SDK
cache is keyed separately from Cargo build output and checked by the fork's
embedded archive/manifest hashes. Native configuration drift fails release CI
instead of silently rebuilding. ORT and zvec build/download behavior is unchanged.
Linux x64 mounts the restored Cargo registry and Git caches into its Docker
builder and returns ownership to the runner before saving them. The host skips
compiler-library/CMake installation; pinned Rust remains for packaging's
`cargo metadata` version lookup. Linux ARM64 still installs its host build tools.

All-platform runs produce `rust-release-<version>`; subset runs produce
`rust-candidate-<version>` with the selected native tarballs and all their components, the same
exact-version meta tarball, checksums and a `release.json` marked `partial`.
Publication and promotion reject partial candidates. After collection,
independent CPU jobs for every selected target download and install the exact
base/meta tarballs with the default `auto` installation mode, without compiling
or repacking. A loopback registry exposes only this run's CUDA archives and
records requests. CPU installation must make zero CUDA metadata/tarball requests,
leave the plugin cache empty and install no CUDA components; optional download
failure followed by CPU fallback is not a pass. No public registry is used.
They verify checksums, CLI startup and daemon IPC. Linux/Windows x64 also verify real
GGUF/ONNX indexing and vector queries with
explicit CPU selection and default CPU fallback, without CUDA host libraries.
These checks also run for unpublished candidates and must pass before npm
publication; failures retain the candidate and `cpu-artifact-evidence-*` logs.

The package GPU jobs use the hosted T4 runners with CUDA runtime libraries,
not Rust/C++/CUDA compiler setup. They verify the downloaded native package's
payload checksums, share the `install-artifacts` installation path with CPU smoke
checks (real meta-package postinstall, no synthetic metadata). Installation runs
inside the Linux CUDA 12 container and after Windows runtime preparation, using
only this run's artifacts. Each GPU job first installs the CPU base in a fresh
prefix and exercises explicit CPU inference. It then runs real
`ZG_INSTALL_ACCELERATOR=cuda npm rebuild` in that same prefix, fetching the
unpublished CUDA archives from a loopback registry. Every plugin must verify,
and the original core identity and binary must remain unchanged.
The rebuilt installation is used for both CUDA 12 fallback and CUDA 13 primary
inference (shared between Linux runtime containers); fresh automatic GPU
installation is also checked separately with `--accelerator auto --expect-cuda`,
so optional download failure cannot pass by leaving only CPU installed.
That fresh installation then runs index/query with `--device` omitted:
GGUF must report CUDA layer offload for both the 12.4 and 13.1 plugins, and ONNX
must report CUDA graph placement in the CUDA 12 environment (also alongside
CUDA 13 on Windows). Explicit CPU isolation and explicit CUDA checks on the
rebuilt prefix remain unchanged. These checks run with
`publish=false,cuda=true` and require real inference evidence, not merely
successful installation. CPU/GPU jobs remain parallel; the GPU job bootstraps
its own CPU installation rather than transferring the CPU runner's prefix.
Both verification paths use the current platform's native artifact plus the
small `rust-verification-<version>` meta/inventory artifact; their installation
reports identify the exact collected hashes. CPU request logs and GPU
before/after receipts plus rebuild request logs are retained as evidence.
Public registry checks run on CPU runners
and validate downloaded component versions/hashes and the reconstructed runtime,
so CPU fallback cannot mask missing or corrupt GPU payloads.
Windows GPU jobs also prepare the pinned NVIDIA 591.59 driver when an older
driver is active, verify the installer signature and CUDA initialization, and fail on a
reboot requirement instead of rebooting. Both Rust CI and package GPU validation
configure the CUDA DLL search paths and check plugin/dependency loading before
inference. These steps prepare temporary CI runners, not end-user npm installs;
see [Windows GPU runner preparation](../CONTRIBUTING.md#windows-gpu-runner-preparation).
`rust-ci.yml` separately verifies the workspace and all
three real models using each platform's already-compiled test binary. Its manual
`cuda_verify` input defaults to `false`. When enabled, Linux/Windows test jobs
compile with `zg/cuda` on CPU runners from the outset, run the normal tests and
CPU models, and upload the selected test executables and verified runtime files.
Separate GPU jobs reuse that bundle for ORT/llama.cpp CUDA verification and both
C5 tests without compiling or restoring build caches.
Model2Vec remains CPU-only and macOS remains non-CUDA. The ordinary quality job
stays separate. `platform=linux` or `windows` limits test platforms; optional
`ref` selects the reviewed source commit. These CI checks do not package or publish.
Both packages share one llama/ggml core and include CUDA 13.1 and CUDA 12.4 GGUF
plugins. The hash-verified official ONNX Runtime CUDA 12 core/providers are
shared-library sidecars. The Windows CUDA package supports CPU and NVIDIA CUDA,
not DirectML; the non-CUDA Windows build retains its existing DirectML runtime.

Explicit publication runs from a `rust-v<version>`-tagged commit on `main` and
uses npm provenance with trusted publishing or `NPM_TOKEN`. Native packages are
published first and the meta package last, always under `rust-canary`. Selecting
`latest` requests the separate promotion job; any registry smoke failure leaves
the existing default installation unchanged.

Publication is currently blocked until reviewed license texts and SPDX SBOMs
exist for every native target under [`compliance/`](compliance/README.md).
Candidate artifacts can still be downloaded, but they are not publication-ready
while these inputs are missing. Both the initial source check and the tarball
preflight fail before publication when compliance inputs are incomplete.

Publication and promotion explicitly install npm 11.21.0, which supports OIDC
authentication for `npm dist-tag`. Configure trusted publishing for all 14
packages with repository `zvec-ai/zvec-grep`, workflow filename
`rust-publish.yml`, and both **Allow npm publish** and **Allow npm dist-tag**.
The dist-tag permission is independent and disabled by default; `id-token:
write` alone does not grant it. See the [npm trusted publishing documentation](https://docs.npmjs.com/trusted-publishers/#managing-dist-tags-with-trusted-publishing).
An eligible `NPM_TOKEN` remains an alternative authentication method.

Developer certificate signing and Apple notarization can be added to the native
build steps independently of npm publishing.
