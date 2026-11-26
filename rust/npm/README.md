# npm distribution

The same verified native payloads also produce complete archives for the
[standalone GitHub Releases installers](../standalone/README.md). These installers
do not require Node/npm on user machines and keep their installation state separate
from npm. Re-run the standalone installer to upgrade; npm installations continue
to use npm.

## Package layout

The release layout has one small meta package, one base native package for each
published target, and exact-version CUDA component packages for Linux/Windows
x64. `platforms.json` is the single source of truth for package
names and npm `os`, `cpu`, and `libc` selectors.

The current published targets are macOS ARM64/x64, Linux x64/ARM64 with glibc,
and Windows x64/ARM64.

## Integration upgrades

After native installation succeeds, eligible per-user global installs run
`zg --install --upgrade-installed` using the installed binary's absolute path.
This migrates existing 0.2.x agent launchers without adding integrations or
starting a daemon. See the [package README](meta/README.md) for skip conditions,
`ZG_INSTALL_SKIP_AGENT_UPGRADE`, backups and manual recovery. Release verification
sets the skip flag so temporary npm prefixes never change user configuration.

## Runtime payloads

After installation, the meta package retains the existing runtime paths. CUDA
files are present only after successful plugin installation:

```text
bin/zg.exe              # meta entry on every OS; command remains zg
lib/libzvec_c_api.*       # storage runtime with embedded Jieba dictionary
lib/lib{llama,ggml}*      # CUDA variant: shared llama.cpp core and CPU backend
lib/*onnxruntime*        # ONNX Runtime core/providers, when dynamically linked
lib/*pdfium*             # LiteParse PDFium for native PDF parsing and rendering
lib/*ggml-cuda*          # primary CUDA 13.1 GGUF plugin
lib/cuda12/*ggml-cuda*    # CUDA 12.4 GGUF fallback plugin
```

Every release package carries the project's `LICENSE`. Native packages also
carry `checksums.json` and any supplied `LICENSES/` or `SBOM.spdx.json`; these
are package metadata rather than runtime files copied by the installer.

The Linux and Windows x64 base packages' `plugins.json` references `-ggml-cuda13`,
`-ggml-cuda12`, `-ort-cuda-part1` and `-ort-cuda-part2` packages. These suffixes
extend the base platform package name. macOS, Linux ARM64 and deliberate CPU-only
builds remain single native packages. A default all-platform release contains 15 tarballs:
six base packages, eight runtime components and the meta package.

The base package retains the executable and CPU/core libraries, including
zvec's embedded Jieba dictionary; no external dictionary files are required.
CUDA components contain whole GGML plugins or fragments of the ORT provider.
Both Linux and Windows ORT providers are split into two ordered byte ranges under
`parts/`; postinstall streams them back into the original `.so` or `.dll` and
verifies the original complete file's SHA256. No compiler, model execution or
CUDA Toolkit installation is performed during npm installation. The meta package
selects its base by OS/CPU/libc; CUDA components are not npm dependencies.
Postinstall first verifies
and installs CPU, then uses bounded NVIDIA detection to decide whether to fetch
the complete pinned CUDA set. CPU-only hosts make no CUDA component requests.
GPU installs still download the whole set; splitting limits each upload, not
their total size. Downloaded archives may occupy cache space in addition to the
assembled libraries.
Auto detection checks only whether `nvidia-smi` succeeds within five seconds
and returns a GPU. It does not compare the reported driver version, validate
CUDA/cuDNN dependencies, or select a per-backend subset of plugins. GGUF and ORT
still choose their execution backends independently at runtime.

## Installation controls

Use `ZG_INSTALL_ACCELERATOR=cpu|auto|cuda` (default `auto`) with npm install or
rebuild. `cpu` skips detection/downloads; `cuda` explicitly requests plugins even
on a GPU-free builder and fails on any plugin error. Auto failures retain CPU
with diagnostics. A forced plugin failure also leaves the CPU core intact.
Later use
`ZG_INSTALL_ACCELERATOR=cuda npm rebuild -g @zvec/zvec-grep --allow-scripts=@zvec/zvec-grep`
(omit `-g` for project-local installs). Stop zg first and restart the daemon afterwards.
There is no extra setup command or runtime downloading.

`ZG_PLUGIN_DIR` is an authoritative absolute directory of exact-version `.tgz`
files. `ZG_INSTALL_OFFLINE=1`/npm offline forbids network requests;
`ZG_PLUGIN_CACHE` overrides the content-addressed download cache. Missing local
files never fall back to a registry version. npm handles registry, proxy, auth
and custom CA settings for online transport, with component scripts disabled.
Archives, payloads and reconstructed libraries are verified before activation;
the cache is not a permanent runtime library path. See [installation and recovery](meta/README.md).

## Package size and notices

Every compressed `.tgz` has a **180 MiB maximum**, enforced after `npm pack`,
during collection, and again before publication/promotion. Plugin validation
uses the same limit. This is a conservative project budget, not an npm guarantee.
npm's base64 encoding expands 180 MiB to 240 MiB before
JSON metadata and provenance. `npm pack` succeeding alone is not publication
validation. A component that outgrows the budget fails closed;
it must be split further, not silently published as a reduced CPU package.

Supplemental `LICENSES/` and `SBOM.spdx.json` inputs are optional for packaging,
publication, and promotion. Existing upstream license and notice files remain
bundled with their native libraries and redistributed ORT components.

## Platforms and native runtimes

`bin/` contains only the native executable. All runtime libraries, providers,
and plugins live under `lib/`. Unix binaries use a `../lib`
rpath. Windows uses a native MSVC delay-load hook to resolve DLLs from the
executable's sibling `lib/`. The binary runs directly, without a JavaScript
launcher or custom loader environment variables for bundled libraries. Host
NVIDIA libraries must still be discoverable by the system loader.

All six targets dynamically load official ORT 1.21.0. The engine build script
stages verified libraries beside Cargo executables, tests and examples; npm
copies them and their upstream notices into `lib/`. No target downloads Pyke
static ORT archives or compiles ORT from source. CPU-only Windows uses the
DirectML NuGet runtime and its pinned DirectML DLL; CUDA x64 bundles use the
matching official CPU core/shared provider/CUDA provider from one distribution.

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
| Linux x64 glibc (glibc 2.28 baseline) | CPU + CUDA 13.1, CUDA 12.4 fallback | CPU + CUDA 12 |
| Linux ARM64 glibc (glibc 2.28 baseline) | CPU | CPU |
| Windows x64 | CPU + CUDA 13.1, CUDA 12.4 fallback | CPU + CUDA 12 |
| Windows ARM64 | CPU | CPU + DirectML |

Linux x64/ARM64 release builders use digest-pinned `manylinux_2_28`
`2026.03.01-1` images. The executable and bundled libraries are audited against
`GLIBC_2.28`; installed artifacts are exercised in clean AlmaLinux 8.10
(glibc 2.28) containers, independently of the host runner's Ubuntu version.
The Linux builds also check offline CPU ORT preparation by executing the real
`zg-engine/build.rs` with `CARGO_FEATURE_VULKAN=1` and `CARGO_NET_OFFLINE=true`
in a network-disabled builder, using cached official CPU ORT and no
`ORT_DYLIB_PATH` or CUDA feature. This uses separate output, does not change
release artifacts and is not a full Vulkan backend compilation.

A deliberate CPU-only Windows build retains DirectML. The published Windows
CUDA bundle uses the official ONNX Runtime 1.21.0 CUDA 12 provider and does not
claim DirectML support. ORT requires CUDA 12 runtime/cuBLAS and cuDNN 9 even
when GGUF uses CUDA 13; the two runtime families can coexist. NVIDIA driver,
CUDA runtime, cuBLAS and cuDNN remain host dependencies, not bundled libraries.
macOS keeps its existing ONNX CPU routing because of CoreML regressions.
Linux musl, CUDA on ARM64/macOS and Vulkan packages are not in
the release matrix.

## CUDA compatibility

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

## Package identity and installation requirements

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
extraction uses system `tar`; local archives need no network. Development and
package tests use the Node version required by the workspace's
[package.json](../package.json), independently of published installation compatibility.

## Local packaging

These scripts require Node.js 20+ and npm in addition to the
[native build prerequisites](../docs/development/building.md). Run them from `rust/`:

```sh
npm run build
npm run install:local
zg --version
```

The install links the active npm global prefix to `dist/npm/zvec-grep-local`;
rerun it after cleaning `dist/`. Use `npm run pack:local` for a self-contained
local tarball without installation, or `npm run test:package` for an isolated
install smoke test. Generated packages live under `dist/npm/`.

The build/pack/install scripts prefer CUDA on Linux/Windows x64. In default
`auto` mode, no `nvcc` on PATH selects a CPU-only build. Use `--cpu-only` to
request CPU explicitly, or prebuilt mode for a CUDA-capable x64 package:

```sh
npm run build -- --cpu-only
# CUDA-capable x64 package without nvcc (PowerShell: set $env:LLAMA_CPP_BUILD_MODE):
LLAMA_CPP_BUILD_MODE=prebuilt npm run build
```

`--no-build` reuses the last successful build; `--prefix <path>` selects an
isolated npm prefix for installation or smoke testing. Manual CPU packaging
rejects directories containing stale CUDA libraries. Use a fresh target
directory or an audited CUDA bundle instead of mixing payloads.

The release commands prepare intermediate native packages used by both
distribution channels:

| Command | Result |
| --- | --- |
| `npm run release:verify` | Check platform/release configuration |
| `npm run release:pack` | Build and pack the current platform |
| `npm run release:cpu` | Build an explicitly CPU-only local package |
| `npm run release:collect -- --targets darwin-arm64` | Collect a validation-only subset, including the meta package and `release.json`; change the target to match your packages |
| `npm run release:collect` | Collect the complete platform/component set |
| `npm run release:artifact-smoke` | Install and verify collected packages on the matching host without rebuilding |

Collected tarballs and `release.json` live under `dist/npm/release/tarballs/`.
Collection verifies exact versions and hashes and rejects missing or extra
tarballs. For CUDA targets it requires every component, even when testing CPU
installation. `--expect-cpu` checks for zero CUDA downloads on CPU hosts;
`--accelerator auto --expect-cuda` requires CUDA installation on GPU hosts.
Pass smoke options after `--` when using npm scripts.

For native GitHub Release archives, continue with the
[standalone release guide](../docs/development/releasing.md).

## Publication policy

The [release guide](../docs/development/releasing.md) describes the shared
workflow through standalone GitHub Release publication. Use
[validation](../docs/development/validation.md) to select local or CI checks.

Release order:

1. Build the selected native targets and collect exact-version
   native/component/meta tarballs with checksums.
2. Run independent CPU and requested GPU artifact checks. Publication requires
   all six targets and both CPU/GPU gates; partial candidates are validation-only.
3. Publish runtime components, then base native packages, then the meta package,
   always under `rust-canary`.
4. Run the six-platform registry install, model, daemon, MCP, upgrade and rollback
   matrix through npm's real command entry points.
5. Promote native `latest` tags and then the meta `latest` tag only after all
   registry gates pass.

`rust-canary` permits publication from `main` or a tag; `latest` requires a
tag. The version comes from Cargo, independently of the tag name. Native
packages use npm provenance with trusted publishing or `NPM_TOKEN`.
A failed registry gate leaves the existing default installation unchanged.

Every package includes the project `LICENSE` and bundled upstream notices.
Supplemental license collections and SPDX SBOMs under
[compliance/](compliance/README.md) are optional packaging inputs.

Publication and promotion use npm 11.21.0 for OIDC dist-tag support. Configure
trusted publishing for all 15 packages with repository `zvec-ai/zvec-grep`,
workflow `rust-publish.yml`, and both **Allow npm publish** and **Allow npm
dist-tag**. The tag permission is separate; workflow `id-token: write` alone
does not grant it. See the
[npm trusted publishing documentation](https://docs.npmjs.com/trusted-publishers/#managing-dist-tags-with-trusted-publishing).
An eligible `NPM_TOKEN` is an alternative.
