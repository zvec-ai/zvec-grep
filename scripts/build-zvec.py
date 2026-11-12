"""Build the pinned zvec C API and Node binding; never substitute a release SDK."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
ZVEC_COMMIT = "06076297c1ca34cc7edc2fa46dff48fc02f6da06"
NODE_COMMIT = "51c542807815a90275a5a14066a575cf991c65d8"


def run(args, **kwargs):
    subprocess.run([str(x) for x in args], check=True, **kwargs)


def output(args, **kwargs):
    return subprocess.check_output([str(x) for x in args], text=True, **kwargs).strip()


def source(path, url, commit):
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        run(["git", "clone", "--no-checkout", url, path])
        run(["git", "-C", path, "checkout", "--detach", commit])
    if output(["git", "-C", path, "rev-parse", "HEAD"]) != commit:
        raise SystemExit(f"Expected {path} at {commit}; refusing a different native runtime")
    if output(["git", "-C", path, "status", "--porcelain", "--untracked-files=no"]):
        raise SystemExit(f"Native source has uncommitted changes: {path}")


def windows_node_sdk(directory, version, arch):
    base = f"https://nodejs.org/download/release/v{version}/"
    with urllib.request.urlopen(base + "SHASUMS256.txt", timeout=60) as response:
        checksums = dict(line.split()[::-1] for line in response.read().decode().splitlines())
    directory.mkdir(parents=True, exist_ok=True)
    for name in [f"node-v{version}-headers.tar.gz", f"win-{arch}/node.lib"]:
        target = directory / name
        target.parent.mkdir(parents=True, exist_ok=True)
        if not target.exists() or hashlib.sha256(target.read_bytes()).hexdigest() != checksums[name]:
            with urllib.request.urlopen(base + name, timeout=120) as response:
                data = response.read()
            if hashlib.sha256(data).hexdigest() != checksums[name]:
                raise SystemExit(f"Node SDK checksum mismatch: {name}")
            target.write_bytes(data)
    with tarfile.open(directory / f"node-v{version}-headers.tar.gz") as archive:
        archive.extractall(directory, filter="data")
    return directory / f"node-v{version}/include/node", directory / f"win-{arch}/node.lib"


def verify_native(directory, arch):
    manifest = json.loads((directory / "manifest.json").read_text())
    if (manifest["zvec_commit"], manifest["node_commit"], manifest["platform"], manifest["arch"]) != (
            ZVEC_COMMIT, NODE_COMMIT, sys.platform, arch):
        raise SystemExit("Native runtime identity mismatch")
    names = {"zvec_node_binding.node", library_name()}
    if sys.platform == "win32":
        names.add("zvec_c_api.lib")
    if set(manifest["files"]) != names:
        raise SystemExit("Native runtime files mismatch")
    for name, expected in manifest["files"].items():
        if hashlib.sha256((directory / name).read_bytes()).hexdigest() != expected:
            raise SystemExit(f"Native runtime checksum mismatch: {name}")
    run(["node", "-e", "const b = require(process.argv[1]); if (b.IndexType.DISKANN === undefined) throw new Error('DiskANN unavailable');",
         directory / "zvec_node_binding.node"])
    print(json.dumps(manifest, indent=2))


def library_name():
    return {"darwin": "libzvec_c_api.dylib", "linux": "libzvec_c_api.so", "win32": "zvec_c_api.dll"}[sys.platform]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=ROOT.parent / "zvec")
    parser.add_argument("--node-source", type=Path, default=ROOT / ".cache/zvec-node")
    parser.add_argument("--build-dir", type=Path, default=ROOT / ".cache/zvec-build")
    parser.add_argument("--output-dir", type=Path, default=ROOT / "native")
    parser.add_argument("--node-headers", type=Path)
    parser.add_argument("--node-library", type=Path)
    parser.add_argument("--verify", action="store_true", help="Verify existing runtime files and load the binding")
    parser.add_argument("--jobs", type=int, default=min(8, os.cpu_count() or 1))
    args = parser.parse_args()
    if sys.platform not in ("darwin", "linux", "win32"):
        raise SystemExit("Supported source-build platforms: macOS, Linux and Windows")
    if args.jobs < 1:
        parser.error("--jobs must be positive")
    arch = output(["node", "-p", "process.arch"])
    destination = args.output_dir.resolve()
    if args.verify:
        verify_native(destination, arch)
        return
    src, node_src, build = (p.resolve() for p in (args.source, args.node_source, args.build_dir))
    source(src, "https://github.com/richyreachy/zvec.git", ZVEC_COMMIT)
    run(["git", "-C", src, "submodule", "update", "--init", "--recursive", "--depth", "1"])
    source(node_src, "https://github.com/zvec-ai/zvec-node.git", NODE_COMMIT)
    node_api = Path(output(["node", "-p", "require.resolve('node-addon-api/package.json')"], cwd=ROOT)).parent
    headers, node_library = args.node_headers, args.node_library
    if sys.platform == "win32" and (headers is None or node_library is None):
        sdk_headers, sdk_library = windows_node_sdk(build / "node-sdk", output(["node", "-p", "process.versions.node"]), arch)
        headers, node_library = headers or sdk_headers, node_library or sdk_library
    headers = headers or Path(shutil.which("node")).resolve().parents[1] / "include/node"
    if not (headers / "node_api.h").is_file():
        raise SystemExit("Node development headers missing; provide --node-headers")
    # CMake re-parses these paths in generated files; Windows backslashes can
    # become escapes such as \a. CMake accepts forward slashes on every host.
    configure = ["cmake", "-S", src.as_posix(), "-B", build.as_posix(), "-DCMAKE_BUILD_TYPE=Release",
                 "-DBUILD_C_BINDINGS=ON", "-DBUILD_PYTHON_BINDINGS=OFF", "-DBUILD_TESTING=OFF",
                 "-DBUILD_TOOLS=OFF", "-DBUILD_CPP_EXAMPLES=OFF", "-DBUILD_ZVEC_SHARED=OFF",
                 "-DBUILD_ZVEC_CORE_SHARED=OFF", "-DBUILD_ZVEC_AILEGO_SHARED=OFF",
                 f"-DCMAKE_PROJECT_zvec_INCLUDE={(ROOT / 'scripts/zvec-node.cmake').as_posix()}",
                 f"-DZG_NODE_SOURCE={node_src.as_posix()}", f"-DZG_NODE_API={node_api.as_posix()}",
                 f"-DZG_NODE_HEADERS={headers.resolve().as_posix()}"]
    if sys.platform == "win32":
        configure += ["-G", "Visual Studio 17 2022", "-A", {"x64": "x64", "arm64": "ARM64"}[arch],
                      f"-DZG_NODE_LIBRARY={node_library.resolve().as_posix()}", "-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded"]
    run(configure)
    run(["cmake", "--build", build, "--config", "Release", "--target", "zvec_c_api", "zvec_node_binding",
         "--parallel", args.jobs])
    artifacts = json.loads((build / "zvec-artifacts-Release.json").read_text())
    destination.mkdir(parents=True, exist_ok=True)
    outputs = {library_name(): artifacts["capi"], "zvec_node_binding.node": artifacts["node"]}
    if sys.platform == "win32":
        outputs["zvec_c_api.lib"] = artifacts["linker"]
    files = {}
    for name, source_file in outputs.items():
        target = destination / name
        shutil.copy2(source_file, target)
        if sys.platform == "darwin":
            run(["codesign", "--force", "--sign", "-", target])
        files[name] = hashlib.sha256(target.read_bytes()).hexdigest()
    shutil.copy2(src / "LICENSE", destination / "LICENSE.zvec")
    shutil.copy2(node_src / "LICENSE", destination / "LICENSE.zvec-node")
    manifest = dict(zvec_commit=ZVEC_COMMIT, node_commit=NODE_COMMIT,
                    platform=sys.platform, arch=arch, files=files)
    (destination / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    verify_native(destination, arch)


if __name__ == "__main__":
    main()
