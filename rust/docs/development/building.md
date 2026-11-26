# Build the native CLI

Start with the default build below. It builds `zg` for your current machine
and is enough for CLI, engine, daemon and MCP development. CUDA is optional.

## 1. Prepare your machine

Install Git, [rustup](https://rustup.rs/), Python 3 and curl. CI uses Python 3.12.
Then prepare the compiler and linker for your platform:

| Platform | Required setup |
| --- | --- |
| macOS, Apple Silicon or Intel | Install Xcode Command Line Tools with `xcode-select --install` if they are not installed. Make `python3` available on PATH; macOS supplies curl. |
| Linux, x86_64 or ARM64 with glibc | Install GCC/G++, make, Python 3, curl and CA certificates. Ubuntu 24.04 is used for Linux workspace CI. See the Debian/Ubuntu commands below. |
| Windows, x64 or ARM64 | Install Visual Studio 2022 Build Tools with **Desktop development with C++**, MSVC and a Windows SDK. Include ARM64 build tools for ARM64. Install a matching native MSVC Rust toolchain through rustup, plus Python on PATH. Use **Developer PowerShell for VS 2022** for the commands below. |

On Debian/Ubuntu:

```sh
sudo apt-get update
sudo apt-get install -y build-essential git python3 curl ca-certificates
```

On Unix the build invokes `python3`; on Windows it invokes `python` and
`curl.exe`. Set `PYTHON` to the path of a Python executable if yours uses
another name.

The first build needs network access to download the pinned Rust toolchain,
Cargo dependencies and native SDK/runtime archives. Matching builds use prebuilt
llama.cpp and zvec SDKs. Rust dependencies still compile C/C++ code, so the
compiler and linker above are required. A GPU and CUDA Toolkit are not needed
for this default build.

## 2. Enter the Rust workspace

For a fresh checkout, run these commands in a directory where you keep source
repositories; they work in a Unix shell or PowerShell:

```sh
git clone https://github.com/zvec-ai/zvec-grep.git
cd zvec-grep/rust
```

If you already have a checkout, open a terminal in its `rust/` directory.
**Run every remaining command in this guide from that directory.**
It contains [Cargo.toml](../../Cargo.toml),
[Cargo.lock](../../Cargo.lock) and [rust-toolchain.toml](../../rust-toolchain.toml).

```sh
rustup show active-toolchain
```

Rustup selects and installs the toolchain pinned by the repository. Use that
toolchain rather than choosing a separate Rust version.

## 3. Build and run

```sh
cargo build --locked -p zg
cargo run --locked -p zg -- --version
cargo run --locked -p zg -- --rg -F "zvec-grep Rust workspace" README.md
```

A successful last command prints the matching README heading. It exercises the
local executable without creating an index, downloading a model or starting a
daemon. `--locked` uses the committed dependency versions.

The executable is `target/debug/zg` on macOS/Linux or
`target/debug/zg.exe` on Windows. To run it directly:

| Shell | Command |
| --- | --- |
| macOS/Linux | `./target/debug/zg --help` |
| Windows PowerShell | `.\target\debug\zg.exe --help` |

Cargo deploys required shared libraries beside the executable. Keep those files
together; copying only `zg` or `zg.exe` is insufficient.

## 4. Build an optimized executable

Use the debug build for routine edits. Use a release build for performance
measurement or regular local use:

```sh
cargo build --locked --release -p zg
cargo run --locked --release -p zg -- --version
```

The executable is now under `target/release/`. `CARGO_TARGET_DIR` or
`--target-dir` changes the output root. The paths in this guide assume the
default output directory unless the command explicitly selects another one.

For development checks, continue with [contributing](../../CONTRIBUTING.md#verification).
For distributable bundles, follow [packaging and releasing](releasing.md).
Cargo output is a development layout and can depend on local SDK cache paths.

## Optional: build with CUDA

CUDA builds are supported on Linux x86_64 and Windows x64. The pinned prebuilt
SDK lets you compile on a machine without a GPU or CUDA compiler.
Use a separate output directory to avoid mixing CPU and CUDA libraries.

Linux:

```sh
LLAMA_CPP_BUILD_MODE=prebuilt cargo build --locked -p zg --features cuda --target-dir target/cuda
./target/cuda/debug/zg --version
```

Windows Developer PowerShell:

```powershell
$env:LLAMA_CPP_BUILD_MODE = 'prebuilt'
cargo build --locked -p zg --features cuda --target-dir target/cuda
.\target\cuda\debug\zg.exe --version
```

Add `--release` for an optimized CUDA build and use `target/cuda/release/`.
`LLAMA_CPP_BUILD_MODE` is a build setting; setting it in PowerShell affects
subsequent builds in that shell.

Running GPU inference additionally requires a compatible NVIDIA driver and host
runtime libraries: GGUF uses the CUDA 13.1 primary or CUDA 12.4 fallback plugin;
ONNX uses CUDA 12 libraries and cuDNN 9. Successful compilation or `--version`
does not validate GPU inference. See [runtime validation](validation.md) for
the actual model checks.

## Native runtime downloads and overrides

These are optional settings for offline builds, custom runtimes and diagnosis.
Leave them unset for your first build.

| Component | Default behavior | Override |
| --- | --- | --- |
| llama.cpp / ggml | Select the SDK matching the locked revision, target and features; cache under `$CARGO_HOME/llama-cpp-runtimes`. | `LLAMA_CPP_BUILD_MODE=prebuilt` requires an SDK; `source` compiles native source. `LLAMA_CPP_RUNTIME_CACHE` selects the cache; `LLAMA_CPP_OFFLINE=1` requires cached SDKs. |
| zvec | The locked `zvec-rust` and `zvec-rust-build` dependencies acquire and stage the storage library. | Use the dependency's build diagnostics when SDK acquisition or source fallback fails. |
| ONNX Runtime | The engine build script downloads, verifies and stages the pinned runtime, including for engine tests. | See the ONNX controls below. |

Custom native source configurations require additional tools, including CMake,
Ninja and libclang, plus the relevant backend SDK. They are not needed for the
matching prebuilt llama SDK path described above.

### ONNX Runtime distributions

[onnxruntime-distributions.json](../../scripts/onnxruntime-distributions.json)
pins the runtime archives and SHA-256 hashes.
[prepare-onnxruntime.py](../../scripts/prepare-onnxruntime.py) stages them
automatically; do not run it separately for an ordinary Cargo build.

- `ZG_ORT_CACHE_DIR` selects the archive cache. By default it is
  `ort-prebuilt/` under the Cargo target directory, or target-triple directory
  when building with `--target`.
- `CARGO_NET_OFFLINE=true` requires valid cached ORT archives. An offline build
  also needs the Rust toolchain, Cargo dependencies and native SDKs cached.
- `ORT_DYLIB_PATH` supplies a custom runtime supporting ORT API 21 and skips
  automatic ORT preparation. Leave it unset to use the pinned distribution.
- ONNX WebGPU requires a custom runtime containing WebGPU; GGUF Vulkan alone
  does not require that override.

## Common build failures

| Symptom | What to check |
| --- | --- |
| `cargo`, `rustc` or `rustup` is not found | Install rustup, reopen the terminal and run `rustup show active-toolchain` from `rust/`. |
| C/C++ compiler or linker is missing | Complete the platform setup above. On Windows, use the matching Developer PowerShell and MSVC architecture. |
| ONNX preparation cannot start Python or curl | Check `python3 --version` and `curl --version` on Unix, or `python --version` and `curl.exe --version` on Windows. Set `PYTHON` if needed. |
| An SDK/runtime download or integrity check fails | Check the URL and error in the build output, connectivity, proxy and certificate configuration; retry after fixing the cause. Offline mode only works with verified cached archives. |
| CMake, bindgen or a CUDA compiler is unexpectedly requested | Check the target, enabled features and `LLAMA_CPP_*` overrides. Custom configurations may select a source build. `LLAMA_CPP_BUILD_MODE=prebuilt` makes an unsupported llama SDK configuration fail explicitly. |
| The executable cannot find a shared library | Run the original Cargo-built executable with its sibling libraries present. Remove stale `ORT_DYLIB_PATH` overrides; do not mix CPU/CUDA outputs or copy only the executable. |

For a detailed build log, repeat the failing command with `-vv`, for example
`cargo build --locked -p zg -vv`.
