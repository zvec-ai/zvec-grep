#!/usr/bin/env python3
"""Stage llama runtime files selected by this Cargo invocation, never a glob.

SDK selection, download and integrity checks belong to the pinned sys crate.
This only relocates runtime files for zg; SDK bindings/import archives are not
end-user payload. Source builds keep their existing CUDA 12 fallback builder.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import tarfile
import tempfile


def digest(path):
    checksum = hashlib.sha256()
    with path.open("rb") as payload:
        for block in iter(lambda: payload.read(1024 * 1024), b""):
            checksum.update(block)
    return checksum.hexdigest()


def cargo_records(messages):
    return [json.loads(line) for line in Path(messages).read_text(encoding="utf-8-sig").splitlines()
            if line.strip()]


def backend_directory(records):
    # This is the safe crate's public BACKENDS_DIR contract, also emitted for
    # fresh Cargo builds. It points either into a verified SDK or source OUT_DIR.
    directories = {Path(value) for record in records
                   if record.get("reason") == "build-script-executed"
                   and "llama-cpp-2" in record.get("package_id", "")
                   for name, value in record.get("env", []) if name == "GGML_BACKENDS_DIR"}
    if len(directories) != 1:
        raise RuntimeError(f"Expected one active llama backend directory from Cargo: {directories}")
    return directories.pop().resolve()


def stage_runtime(messages, destination, *, include_fallback=True):
    records = cargo_records(messages)
    backend = backend_directory(records)
    windows = (backend / "ggml-cuda.dll").is_file()
    plugin = "ggml-cuda.dll" if windows else "libggml-cuda.so"
    if not (backend / plugin).is_file():
        raise RuntimeError(f"Missing selected CUDA plugin: {backend / plugin}")
    manifest_path = backend.parent / "manifest.json"
    manifest = None
    if manifest_path.is_file():
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        architectures = os.environ.get("CUDA12_ARCHITECTURES")
        if architectures and architectures != manifest["fallback_cmake"]["CMAKE_CUDA_ARCHITECTURES"]:
            raise RuntimeError("CUDA12_ARCHITECTURES differs from the SDK; use LLAMA_CPP_BUILD_MODE=source")
    elif os.environ.get("LLAMA_CPP_BUILD_MODE") in ("prebuilt", "external"):
        raise RuntimeError("Required prebuilt runtime manifest is missing")

    if manifest:
        core_dirs = [backend]
    else:
        outputs = {Path(record["out_dir"]) for record in records
                   if record.get("reason") == "build-script-executed"
                   and "llama-cpp-sys-2" in record.get("package_id", "")}
        if len(outputs) != 1:
            raise RuntimeError(f"Expected one active llama sys OUT_DIR: {outputs}")
        output = outputs.pop()
        core_dirs = [output / suffix for suffix in ("lib", "lib64", "bin", "build/bin", "build/bin/Release")]

    files = {}
    for stem in ("llama", "ggml", "ggml-base"):
        pattern = f"{stem}.dll" if windows else f"lib{stem}.so*"
        candidates = sorted({p.resolve() for directory in core_dirs for p in directory.glob(pattern)
                             if p.is_file()})
        if not candidates:
            raise RuntimeError(f"Missing selected core library: {pattern}")
        # SDKs contain identical copies of Unix linker-name/SONAME aliases.
        if len({digest(p) for p in candidates}) != 1:
            raise RuntimeError(f"Conflicting core library aliases: {candidates}")
        source = candidates[0]
        name = source.name
        if not windows:
            dynamic = subprocess.check_output(["readelf", "-d", str(source)], text=True)
            soname = re.search(r"\(SONAME\).*\[([^]]+)\]", dynamic)
            if not soname or "/" in soname[1] or "\\" in soname[1]:
                raise RuntimeError(f"Invalid or missing SONAME: {source}")
            name = soname[1]
        files[name] = source
    for source in backend.iterdir():
        if source.is_file() and re.fullmatch(
                r"ggml-(?:cpu(?:-[\w-]+)?|cuda)\.dll" if windows
                else r"libggml-(?:cpu(?:-[\w-]+)?|cuda)\.so(?:\.\d+)*", source.name):
            files[source.name] = source
    if not any("ggml-cpu" in name for name in files):
        raise RuntimeError("Selected runtime has no CPU backend")
    if manifest and include_fallback:
        fallback = backend / "cuda12" / plugin
        if not fallback.is_file():
            raise RuntimeError(f"SDK is missing its CUDA 12 fallback: {fallback}")
        files[f"cuda12/{plugin}"] = fallback

    for name, source in files.items():
        if manifest:
            relative = source.relative_to(backend.parent).as_posix()
            if digest(source) != manifest["files"].get(relative):
                raise RuntimeError(f"SDK runtime payload changed after Cargo verification: {relative}")
        target = Path(destination) / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)
    print(f"Staged {len(files)} llama runtime files from {'verified SDK' if manifest else 'source build'}: {backend}")


def stage_test_bundle(messages, archive):
    """Transfer only the two GPU test executables and their runtime dependencies.

    GPU jobs do not need Cargo, SDK import archives, build output or the CUDA 12
    GGUF fallback (covered by publish CI). The bundle layout uses relative paths;
    the consumer selects lib/ explicitly instead of the compiled-in SDK path.
    """
    records = cargo_records(messages)
    binaries = {}
    for name in ("zg_engine", "zg_llama_native"):
        matches = [record for record in records
                   if record.get("reason") == "compiler-artifact"
                   and record["target"]["name"] == name
                   and record["profile"]["test"] and record.get("executable")]
        if len(matches) != 1:
            raise RuntimeError(f"Expected one current {name} test executable")
        if name == "zg_engine" and "cuda" not in matches[0]["features"]:
            raise RuntimeError("GPU test bundle requires CUDA-enabled engine tests")
        binaries[name] = Path(matches[0]["executable"])
    windows = binaries["zg_engine"].suffix == ".exe"
    profile = binaries["zg_engine"].parent.parent
    zvec = "zvec_c_api.dll" if windows else "libzvec_c_api.so"
    # Only the active zvec build script's link directories, never stale OUT_DIRs.
    zvec_dirs = {Path(value.removeprefix("native=")) for record in records
                 if record.get("reason") == "build-script-executed"
                 and "zvec-rust-sys" in record.get("package_id", "")
                 for value in record.get("linked_paths", [])
                 if value.startswith("native=")}
    zvec_files = {directory / zvec for directory in zvec_dirs if (directory / zvec).is_file()}
    if not zvec_files or len({digest(path) for path in zvec_files}) != 1:
        raise RuntimeError(f"Missing or conflicting current zvec library: {zvec_files}")
    ort = (["onnxruntime.dll", "onnxruntime_providers_shared.dll", "onnxruntime_providers_cuda.dll"]
           if windows else
           ["libonnxruntime.so.1", "libonnxruntime_providers_shared.so", "libonnxruntime_providers_cuda.so"])
    archive.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=archive.parent) as temporary:
        bundle = Path(temporary) / "cuda-tests"
        (bundle / "bin").mkdir(parents=True)
        stage_runtime(messages, bundle / "lib", include_fallback=False)
        for name, binary in binaries.items():
            shutil.copy2(binary, bundle / "bin" / (name + (".exe" if windows else "")))
        shutil.copy2(sorted(zvec_files)[0], bundle / "lib" / zvec)
        for name in ort:
            shutil.copy2(profile / name, bundle / "lib" / name)
        # Record identity and transport integrity without leaking builder paths.
        manifest = {
            "source_sha": os.environ["SOURCE_SHA"],
            "platform": "windows" if windows else "linux",
            "files": {path.relative_to(bundle).as_posix(): digest(path)
                      for path in sorted(bundle.rglob("*")) if path.is_file()},
        }
        (bundle / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        with tarfile.open(archive, "w:gz", compresslevel=3) as package:
            package.add(bundle, arcname="cuda-tests")
    print(f"GPU test bundle: {archive} ({archive.stat().st_size} bytes; no compiler or build cache)")


def verify_test_bundle(bundle):
    manifest = json.loads((bundle / "manifest.json").read_text(encoding="utf-8"))
    if manifest["source_sha"] != os.environ["SOURCE_SHA"]:
        raise RuntimeError("GPU test bundle does not match the reviewed source commit")
    platform = "windows" if os.name == "nt" else "linux"
    if manifest["platform"] != platform:
        raise RuntimeError("GPU test bundle platform mismatch")
    actual = {path.relative_to(bundle).as_posix(): digest(path)
              for path in bundle.rglob("*") if path.is_file() and path != bundle / "manifest.json"}
    if actual != manifest["files"]:
        raise RuntimeError("GPU test bundle file inventory or SHA-256 mismatch")
    for name in ("zg_engine", "zg_llama_native"):
        binary = bundle / "bin" / (name + (".exe" if platform == "windows" else ""))
        if not binary.is_file():
            raise RuntimeError(f"GPU test executable missing: {binary}")
    print(f"Verified {len(actual)} GPU test payloads for {manifest['source_sha']}")


def cargo_runtime_file(name):
    return bool(re.fullmatch(r"(?:lib)?(?:llama[\w-]*|ggml[\w-]*|onnxruntime[\w-]*|zvec_c_api)"
                             r"(?:\.so(?:\.\d+)*|\.dll)", name))


def reset_cargo_cli(profile):
    """Discard only known runtime outputs from the restored CI build cache.

    Otherwise last run's runtime.py copies could hide a broken Cargo build.rs.
    Cargo must recreate these files itself; never copy replacements here.
    """
    if (profile.resolve() != (Path.cwd() / "target/debug").resolve()
            or profile.is_symlink() or profile.parent.is_symlink()):
        raise RuntimeError("Only the current workspace's target/debug CI outputs may be reset")
    for directory in (profile, profile / "deps", profile / "examples", profile / "cuda12"):
        if directory.is_symlink():
            raise RuntimeError(f"Refusing to reset a linked CI output directory: {directory}")
        if directory.is_dir():
            for path in directory.iterdir():
                if cargo_runtime_file(path.name) and (path.is_file() or path.is_symlink()):
                    path.unlink()


def checked_cargo_sdk_root(root, windows):
    # The compiled-in SDK path must survive transfer from a CPU to a GPU runner.
    # Bound restoration to our dedicated CI cache, never an arbitrary artifact path.
    prefix = "C:/zg-ci-runtimes/" if windows else "/work/rust/target/llama-runtimes/"
    normalized = str(root).replace("\\", "/")
    if not re.fullmatch(re.escape(prefix) + r"[0-9a-f]{64}/bundle", normalized, re.IGNORECASE if windows else 0):
        raise RuntimeError(f"Unexpected Cargo CI SDK cache location: {root}")
    return normalized


def archive_cargo_cli(messages, archive):
    """Snapshot Cargo's actual output before any test/packaging runtime staging.

    No file is taken from the SDK to fill a missing profile library. The SDK's
    dynamic backends travel separately, at their original compiled-in location.
    """
    records = cargo_records(messages)
    matches = [r for r in records if r.get("reason") == "compiler-artifact"
               and r["target"]["name"] == "zg" and "bin" in r["target"]["kind"]
               and not r["profile"]["test"] and r.get("executable")]
    if len(matches) != 1 or "cuda" not in matches[0]["features"]:
        raise RuntimeError("Expected exactly one cargo build --features cuda executable")
    binary = Path(matches[0]["executable"]).resolve(strict=True)
    windows = binary.suffix == ".exe"
    sdk = backend_directory(records).parent
    sdk_root = checked_cargo_sdk_root(sdk, windows)
    sdk_manifest = json.loads((sdk / "manifest.json").read_text(encoding="utf-8"))
    files = {f"profile/{binary.name}": binary}
    files.update({f"profile/{p.name}": p for p in binary.parent.iterdir()
                  if p.is_file() and cargo_runtime_file(p.name)})
    files["sdk/manifest.json"] = sdk / "manifest.json"
    for name, checksum in sdk_manifest["files"].items():
        # Rust CUDA CI exercises the primary; publish CI covers CUDA 12 fallback.
        # Do not transfer the unused large fallback payload to GPU runners.
        if name.startswith("lib/cuda12/"):
            continue
        if not cargo_runtime_file(Path(name).name):
            continue
        source = sdk / name
        if not source.resolve().is_relative_to(sdk) or digest(source) != checksum:
            raise RuntimeError(f"Cargo SDK runtime integrity mismatch: {name}")
        files[f"sdk/{name}"] = source
    manifest = {"source_sha": os.environ["SOURCE_SHA"], "sdk_root": sdk_root,
                "platform": "windows" if windows else "linux", "binary": f"profile/{binary.name}",
                "files": {name: digest(path) for name, path in sorted(files.items())}}
    archive.parent.mkdir(parents=True, exist_ok=True)
    # Writing an archive only: never patch RPATH, change the CLI, or populate its
    # sibling directory. GPU extraction reproduces exactly this file inventory.
    with tempfile.TemporaryDirectory(dir=archive.parent) as temporary:
        manifest_path = Path(temporary) / "manifest.json"
        manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        with tarfile.open(archive, "w:gz", compresslevel=3, dereference=True) as package:
            for name, path in sorted(files.items()):
                package.add(path, arcname=f"cargo-cli/{name}", recursive=False)
            package.add(manifest_path, arcname="cargo-cli/manifest.json")
    print(f"Unmodified Cargo CLI snapshot: {archive}; SDK backends retain {sdk_root}")


def verify_cargo_cli(bundle):
    manifest = json.loads((bundle / "manifest.json").read_text(encoding="utf-8"))
    if manifest["source_sha"] != os.environ["SOURCE_SHA"]:
        raise RuntimeError("Cargo CLI snapshot does not match the reviewed source commit")
    windows = os.name == "nt"
    if manifest["platform"] != ("windows" if windows else "linux"):
        raise RuntimeError("Cargo CLI snapshot platform mismatch")
    checked_cargo_sdk_root(manifest["sdk_root"], windows)
    expected_binary = "profile/zg.exe" if windows else "profile/zg"
    if manifest["binary"] != expected_binary or expected_binary not in manifest["files"]:
        raise RuntimeError("Cargo CLI snapshot executable is missing")
    actual = {p.relative_to(bundle).as_posix(): digest(p) for p in bundle.rglob("*")
              if p.is_file() and p != bundle / "manifest.json"}
    if actual != manifest["files"]:
        raise RuntimeError("Cargo CLI snapshot file inventory or SHA-256 mismatch")
    return manifest


def cargo_cli_linux_mounts(workspace, bundle, manifest):
    """Create only empty mountpoints before making the workspace read-only.

    Docker cannot create nested mountpoints under an already read-only /work.
    Keep the original profile and SDK snapshots separate and read-only; do not
    copy libraries or change the SDK's compiled-in location.
    """
    if manifest["platform"] != "linux":
        raise RuntimeError("Linux Cargo CLI mounts require a Linux snapshot")
    sdk_root = checked_cargo_sdk_root(manifest["sdk_root"], False)
    workspace = Path(workspace).resolve()
    mounts = [(bundle / "profile", PurePosixPath("/work/rust/target/debug")),
              (bundle / "sdk", PurePosixPath(sdk_root))]
    for _, destination in mounts:
        (workspace / destination.relative_to("/work")).mkdir(parents=True, exist_ok=True)
    return [f"{workspace}:/work:ro", *[f"{source}:{destination}:ro" for source, destination in mounts]]


def restore_cargo_sdk(bundle):
    """Restore only the existing SDK cache; never supplement profile/ libraries."""
    manifest = verify_cargo_cli(bundle)
    if os.name != "nt":
        raise RuntimeError("Linux mounts the SDK snapshot at its original container path instead")
    root = Path(manifest["sdk_root"])
    for source in (bundle / "sdk").rglob("*"):
        if not source.is_file():
            continue
        destination = root / source.relative_to(bundle / "sdk")
        if destination.exists():
            if digest(destination) != digest(source):
                raise RuntimeError(f"Existing GPU runner SDK cache differs: {destination}")
        else:
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, destination)
    print(f"Restored original Cargo SDK cache (no profile repairs): {root}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cargo-messages", type=Path)
    parser.add_argument("--destination", type=Path)
    parser.add_argument("--test-archive", type=Path)
    parser.add_argument("--cargo-cli-archive", type=Path)
    parser.add_argument("--reset-cargo-cli", type=Path)
    parser.add_argument("--verify-test-bundle", type=Path)
    args = parser.parse_args()
    if args.reset_cargo_cli:
        reset_cargo_cli(args.reset_cargo_cli)
    elif args.cargo_messages and args.cargo_cli_archive:
        archive_cargo_cli(args.cargo_messages, args.cargo_cli_archive)
    elif args.verify_test_bundle:
        verify_test_bundle(args.verify_test_bundle)
    elif args.cargo_messages and args.test_archive:
        stage_test_bundle(args.cargo_messages, args.test_archive)
    elif args.cargo_messages and args.destination:
        stage_runtime(args.cargo_messages, args.destination)
    else:
        parser.error("provide --verify-test-bundle or --cargo-messages with --destination/--test-archive")
