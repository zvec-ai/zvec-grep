#!/usr/bin/env python3
"""Exercise a CUDA package on a real GPU or a clean CPU host (Linux and Windows)."""

import argparse
import ctypes
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


MODELS = (
    ("gguf", "local/embeddinggemma-300m"),
    ("onnx", "local/all-minilm-l6-v2"),
)
MARKER = "cuda_cli_e2e_marker"


def configure_windows_cuda_paths(root):
    """CUDA 13 puts runtime DLLs in bin/x64; older toolkits use bin."""
    root = Path(root).resolve(strict=True)
    directories = [directory for directory in (root / "bin" / "x64", root / "bin")
                   if directory.is_dir()]
    if not directories:
        raise RuntimeError(f"No CUDA runtime directories under {root}")
    paths = [str(directory) for directory in directories]
    existing = os.environ.get("PATH", "").split(os.pathsep)
    added = {path.casefold() for path in paths}
    os.environ["PATH"] = os.pathsep.join(paths + [path for path in existing
                                                if path and path.casefold() not in added])
    # The current Python process and subsequent Actions steps both need the fix.
    if os.environ.get("GITHUB_PATH"):
        with open(os.environ["GITHUB_PATH"], "a", encoding="utf-8") as output:
            output.write("\n".join(paths) + "\n")
    print(f"CUDA DLL search directories: {paths}", flush=True)


def verify_windows_cuda_plugin(plugin, cuda_major, logs, library_directories=()):
    """Load each dependency and the plugin in separate processes, without inference."""
    if os.name != "nt":
        raise RuntimeError("Windows CUDA DLL validation requires Windows")
    plugin = Path(plugin).resolve()
    logs = Path(logs)
    logs.mkdir(parents=True, exist_ok=True)
    environment = dict(os.environ)
    # Packaged zg adds its sibling lib directory through its Windows DLL hook.
    # Rust test executables instead inherit the directories prepared by Rust CI.
    paths = [str(Path(directory).resolve()) for directory in library_directories]
    environment["PATH"] = os.pathsep.join([*paths, environment.get("PATH", "")])
    evidence = {"plugin": str(plugin), "cuda_major": cuda_major,
                "path": environment["PATH"], "checks": []}
    probe = """
import ctypes, sys
library = ctypes.CDLL(sys.argv[1], winmode=0)
if sys.argv[2] == 'plugin':
    getattr(library, 'ggml_backend_init')
kernel32 = ctypes.WinDLL('kernel32', use_last_error=True)
get_path = kernel32.GetModuleFileNameW
get_path.argtypes = [ctypes.c_void_p, ctypes.c_wchar_p, ctypes.c_uint]
get_path.restype = ctypes.c_uint
path = ctypes.create_unicode_buffer(32768)
if not get_path(library._handle, path, len(path)):
    raise ctypes.WinError(ctypes.get_last_error())
print(path.value)
"""
    failures = []
    try:
        if not plugin.is_file():
            raise RuntimeError(f"CUDA plugin was not built or packaged: {plugin}")
        libraries = ["nvcuda.dll", f"cudart64_{cuda_major}.dll",
                     f"cublasLt64_{cuda_major}.dll", f"cublas64_{cuda_major}.dll", str(plugin)]
        for library in libraries:
            # Never preload dependencies in the plugin's process: that could mask
            # a broken search path. winmode=0 matches llama.cpp's LoadLibraryW.
            try:
                result = subprocess.run([sys.executable, "-c", probe, library,
                                         "plugin" if library == str(plugin) else "runtime"],
                                        env=environment, capture_output=True, text=True,
                                        timeout=60, check=False)
                check = {"library": library, "returncode": result.returncode,
                         "stdout": result.stdout, "stderr": result.stderr}
            except subprocess.TimeoutExpired:
                check = {"library": library, "returncode": None,
                         "stdout": "", "stderr": "DLL loading timed out after 60 seconds"}
            evidence["checks"].append(check)
            print(json.dumps(check, ensure_ascii=False), flush=True)
            if check["returncode"] != 0:
                failures.append(library)
        if failures:
            raise RuntimeError(f"Windows CUDA DLL loading failed: {', '.join(failures)}")
    except (OSError, RuntimeError) as error:
        evidence["error"] = str(error)
        raise
    finally:
        (logs / "windows-cuda-load.json").write_text(
            json.dumps(evidence, indent=2), encoding="utf-8")


def cuda_evidence(stderr, backend, expected_gguf_cuda=None):
    """Selection alone is insufficient for ORT: require CUDA graph placement."""
    if re.search(r"fall(?:ing)? back to CPU", stderr, re.IGNORECASE):
        raise AssertionError("CUDA execution fell back to CPU")
    if backend == "gguf":
        if expected_gguf_cuda and not any(
            "llama.cpp CUDA backend library selected" in line
            and re.search(r'cuda_version=["\']?' + re.escape(expected_gguf_cuda) + r'\b', line)
            for line in stderr.splitlines()
        ):
            raise AssertionError(f"Expected GGUF CUDA {expected_gguf_cuda} plugin was not selected")
        lines = [line for line in stderr.splitlines()
                 if "llama.cpp GPU embedding backend selected" in line
                 and re.search(r'backend=["\']?CUDA\b', line, re.IGNORECASE)]
        offload = [line for line in stderr.splitlines()
                   if re.search(r"offloaded [1-9]\d*/\d+ layers to GPU", line)]
        if not offload:
            raise AssertionError("GGUF did not report any layers offloaded to GPU")
        if not lines:
            raise AssertionError("GGUF offload must use CUDA, not another GPU backend")
        lines += offload
    else:
        if "Transformers ONNX cuda embedding execution provider selected" not in stderr:
            raise AssertionError("ONNX CUDA execution provider was not selected")
        lines = [line for line in stderr.splitlines()
                 if re.search(r"placed on \[CUDAExecutionProvider\]\. Number of nodes: [1-9]\d*", line)]
    if not lines:
        raise AssertionError(f"No actual {backend} CUDA execution evidence")
    return lines


def cpu_evidence(stderr, backend, device):
    if ("GPU embedding backend selected" in stderr
            or re.search(r"offloaded [1-9]\d*/\d+ layers to GPU", stderr)
            or re.search(r"placed on \[CUDAExecutionProvider\]\. Number of nodes: [1-9]\d*", stderr)):
        raise AssertionError("CPU-only validation unexpectedly selected a GPU")
    evidence = []
    if backend == "onnx":
        selected = "Transformers ONNX cpu embedding execution provider selected"
        require(stderr, selected)
        evidence.append(selected)
    if device == "default":
        fallback = [line for line in stderr.splitlines()
                    if re.search(r"fall(?:ing)? back to CPU", line, re.IGNORECASE)]
        if not fallback:
            raise AssertionError("Default CUDA package did not report CPU fallback")
        evidence.extend(fallback)
    return evidence or ["Explicit CPU inference completed without GPU offload"]


def verify_cpu_host(logs):
    libraries = (("nvcuda.dll", "cudart64_12.dll", "cudart64_13.dll",
                  "cublas64_12.dll", "cublas64_13.dll", "cudnn64_9.dll") if os.name == "nt" else
                 ("libcuda.so.1", "libcudart.so.12", "libcudart.so.13",
                  "libcublas.so.12", "libcublas.so.13", "libcudnn.so.9"))
    available = []
    for library in libraries:
        try:
            if os.name == "nt":
                # Include PATH in the search, as the packaged executable does.
                ctypes.CDLL(library, winmode=0)
            else:
                ctypes.CDLL(library)
            available.append(library)
        except OSError:
            pass
    evidence = {"checked_libraries": libraries, "available_cuda_libraries": available}
    (logs / "cpu-environment.json").write_text(json.dumps(evidence, indent=2), encoding="utf-8")
    if available:
        raise AssertionError(f"CPU artifact validation requires no loadable CUDA host libraries: {available}")


def require(text, expected):
    if expected not in text:
        raise AssertionError(f"Expected output not found: {expected}")


def run_zg(binary, workspace, logs, stage, args, env):
    command = [str(binary), *args]
    print(f"Running {stage}: {command}", flush=True)
    # Files, rather than PIPE, retain partial output even on timeout/crash.
    stdout_path = logs / f"{stage}.stdout"
    stderr_path = logs / f"{stage}.stderr"
    with stdout_path.open("w", encoding="utf-8") as stdout, stderr_path.open("w", encoding="utf-8") as stderr:
        result = subprocess.run(command, cwd=workspace, env=env, stdout=stdout,
                                stderr=stderr, timeout=900, check=False)
    out = stdout_path.read_text(encoding="utf-8", errors="replace")
    err = stderr_path.read_text(encoding="utf-8", errors="replace")
    if result.returncode:
        raise AssertionError(f"{stage} exited with {result.returncode}\n{out}\n{err[-24000:]}")
    return out, err


def case_environment(work, backend):
    root = work.resolve() / backend
    env = dict(os.environ)
    env.update({
        # The registry is independent of ZVEC_GREP_HOME and XDG_CONFIG_HOME.
        # Each invocation/backend must own its registry, including on Windows.
        "ZVEC_GREP_WORKSPACE_REGISTRY": str(root / "workspaces.json"),
        "ZVEC_GREP_HOME": str(root / "runtime"),
        "ORT_LOG": "verbose",
        "RUST_LOG": "warn,zg=debug,zg_engine=debug,ort=trace,llama-cpp-2=info",
        "NO_COLOR": "1",
    })
    return env


def run_case(binary, model_cache, work, logs, backend, model, expected_gguf_cuda, device="cuda"):
    workspace = work / backend / "workspace"
    workspace.mkdir(parents=True)
    logs.mkdir(parents=True)
    (workspace / "model-fixture.md").write_text(
        "# Model validation fixture\n"
        "The heliotrope capacitor calibrates the observatory telescope.\n"
        f"{MARKER}\n", encoding="utf-8")
    env = case_environment(work, backend)
    runtime_args = ["--model-cache", str(model_cache), "--debug", "--no-color"]
    if device != "default":
        runtime_args += ["--device", device]
    def evidence(stderr):
        if device == "cuda":
            return cuda_evidence(stderr, backend, expected_gguf_cuda)
        return cpu_evidence(stderr, backend, device)
    _, index_err = run_zg(binary, workspace, logs, "index", [
        "--index", "--name", f"{device}-e2e-{backend}", "--mode", "direct",
        "--embedding", model, "--index-embedding-concurrency", "2", *runtime_args,
    ], env)
    require(index_err, "Index complete")
    index_evidence = evidence(index_err)
    status, _ = run_zg(binary, workspace, logs, "status", [
        "--status", "--mode", "direct", "--check-ready", "--no-color",
    ], env)
    require(status, "Workspace index is ready")
    require(status, model)
    query, query_err = run_zg(binary, workspace, logs, "query", [
        "--mode", "direct", "--vector", "Which component calibrates the telescope?",
        "--refresh", "off", "--limit", "1", "--preview", "full", *runtime_args,
    ], env)
    require(query, "model-fixture.md")
    require(query, MARKER)
    query_evidence = evidence(query_err)
    result = {"model": model, "device": device,
              "gguf_cuda": expected_gguf_cuda if backend == "gguf" and device == "cuda" else None,
              "index": index_evidence, "query": query_evidence}
    print(json.dumps(result, indent=2), flush=True)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--model-cache", type=Path, required=True)
    parser.add_argument("--logs", type=Path, required=True)
    parser.add_argument("--backend", choices=("all", "gguf", "onnx"), default="all")
    parser.add_argument("--expected-gguf-cuda", choices=("12.4", "13.1"))
    parser.add_argument("--cpu-only", action="store_true",
                        help="Require a clean CPU host; test explicit CPU and default automatic fallback")
    args = parser.parse_args()
    if args.cpu_only and args.expected_gguf_cuda:
        parser.error("--cpu-only cannot be combined with --expected-gguf-cuda")
    if not args.cpu_only and not args.expected_gguf_cuda:
        parser.error("GPU validation requires --expected-gguf-cuda")
    bundle = args.bundle.resolve()
    package_json = bundle / "package.json"
    metadata = json.loads(package_json.read_text(encoding="utf-8")) if package_json.is_file() else {}
    entry = metadata.get("bin", {})
    entry = entry if isinstance(entry, str) else entry.get("zg")
    binary = (bundle / (entry or f"bin/{'zg.exe' if os.name == 'nt' else 'zg'}")).resolve()
    if not binary.is_relative_to(bundle):
        parser.error("packaged CLI must be inside the installed package")
    lib_dir = bundle / "lib"
    if not binary.is_file():
        parser.error(f"packaged CLI is missing: {binary}")
    if not lib_dir.is_dir():
        parser.error(f"packaged libraries are missing: {lib_dir}")
    logs = args.logs.resolve()
    logs.mkdir(parents=True, exist_ok=True)
    args.model_cache.mkdir(parents=True, exist_ok=True)
    if args.cpu_only:
        verify_cpu_host(logs)
    else:
        gpu = subprocess.check_output([
            "nvidia-smi", "--query-gpu=name,driver_version", "--format=csv,noheader",
        ], encoding="utf-8")
        (logs / "gpu.txt").write_text(gpu, encoding="utf-8")
        print(gpu, flush=True)
        if os.name == "nt":
            plugin_dir = lib_dir / "cuda12" if args.expected_gguf_cuda == "12.4" else lib_dir
            verify_windows_cuda_plugin(plugin_dir / "ggml-cuda.dll",
                                       args.expected_gguf_cuda.split(".")[0], logs,
                                       library_directories=(lib_dir,))
    results, failures = {}, []
    devices = ("cpu", "default") if args.cpu_only else ("cuda",)
    # Windows runners have a short RUNNER_TEMP; the user-profile temp directory
    # leaves too little room for zvec's nested index paths. Keep Linux unchanged.
    temporary_parent = (os.environ.get("RUNNER_TEMP") or None) if os.name == "nt" else None
    temporary_prefix = "zg-" if os.name == "nt" else "zg-cuda-e2e-"
    with tempfile.TemporaryDirectory(prefix=temporary_prefix, dir=temporary_parent) as temporary:
        for device in devices:
            for backend, model in MODELS:
                if args.backend not in ("all", backend):
                    continue
                case = backend if device == "cuda" else f"{device}/{backend}"
                try:
                    results[case] = run_case(binary, args.model_cache.resolve(), Path(temporary) / device,
                                            logs / case, backend, model, args.expected_gguf_cuda, device)
                except (AssertionError, OSError, subprocess.SubprocessError) as error:
                    failures.append(f"{case}: {error}")
                    print(f"FAILED: {failures[-1]}", flush=True)
    (logs / "results.json").write_text(json.dumps({"passed": results, "failures": failures}, indent=2),
                                      encoding="utf-8")
    if failures:
        raise SystemExit("Packaged CLI end-to-end validation failed (see retained logs)")
    print(f"Packaged CLI end-to-end validation passed: {', '.join(results)} index + query", flush=True)


if __name__ == "__main__":
    main()
