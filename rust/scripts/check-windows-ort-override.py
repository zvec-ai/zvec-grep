#!/usr/bin/env python3
"""Exercise custom ORT sibling DLL lookup in a relocated Windows CLI package."""

import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).parent / "cuda"))
from e2e import cargo_environment


def check(bundle, model_cache, logs):
    if os.name != "nt":
        raise RuntimeError("This check requires Windows")
    logs.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="zg-ort-override-") as temporary:
        root = Path(temporary)
        installed = root / "installed"
        shutil.copytree(bundle, installed)
        custom = root / "custom ORT 外部"
        custom.mkdir()
        for name in ("onnxruntime.dll", "DirectML.dll"):
            shutil.move(installed / "lib" / name, custom / name)
            # A fallback to the packaged copy must fail, not mask the override.
            (installed / "lib" / name).write_bytes(b"invalid packaged DLL")
        binary = installed / "bin/zg.exe"
        # Cargo-style adjacent dependencies normally precede SetDllDirectory.
        # The explicitly selected runtime must still supply its own DirectML.
        (binary.parent / "DirectML.dll").write_bytes(b"invalid adjacent DLL")
        base = cargo_environment(binary, os.environ)
        base.update(RUST_LOG="warn,zg_engine=debug", ORT_LOG="verbose")
        cases = {
            "file": {"ORT_DYLIB_PATH": str(custom / "onnxruntime.dll")},
            "directory": {"ZVEC_GREP_NATIVE_LIB_DIR": str(custom)},
            "relative-file": {"ORT_DYLIB_PATH": os.path.relpath(custom / "onnxruntime.dll", binary.parent)},
            "relative-directory": {"ZVEC_GREP_NATIVE_LIB_DIR": os.path.relpath(custom, binary.parent)},
        }
        for label, override in cases.items():
            workspace = root / "work" / label
            workspace.mkdir(parents=True)
            (workspace / "fixture.md").write_text("The heliotrope capacitor calibrates the observatory telescope.\n")
            environment = {**base, **override, "ZVEC_GREP_HOME": str(root / "home" / label)}
            common = ["--mode", "direct", "--device", "cpu", "--model-cache", str(model_cache), "--debug", "--no-color"]
            commands = {
                "index": ["--index", "--embedding", "local/all-minilm-l6-v2", *common],
                "query": ["--vector", "Which component calibrates the telescope?", "--refresh", "off", "--limit", "1", *common],
            }
            for stage, arguments in commands.items():
                result = subprocess.run([str(binary), *arguments], cwd=workspace, env=environment,
                                        capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=180)
                (logs / f"{label}-{stage}.log").write_text(result.stdout + result.stderr, encoding="utf-8")
                if result.returncode or "Transformers ONNX cpu embedding execution provider selected" not in result.stderr:
                    raise RuntimeError(f"Custom ORT {label}/{stage} failed: {result.returncode}\n{result.stderr}")
                if stage == "query" and "fixture.md" not in result.stdout:
                    raise RuntimeError(f"Custom ORT query returned no fixture: {result.stdout}")
            print(f"Custom ORT sibling lookup passed: {label}", flush=True)
        # An invalid selected sibling must not be silently replaced by a system
        # DirectML DLL. Each CLI invocation is a fresh process with no loaded ORT.
        (custom / "DirectML.dll").write_bytes(b"invalid selected DLL")
        result = subprocess.run([str(binary), *commands["query"]], cwd=workspace,
                                env={**base, **cases["file"], "ZVEC_GREP_HOME": str(root / "home" / label)},
                                capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=180)
        (logs / "invalid-sibling.log").write_text(result.stdout + result.stderr, encoding="utf-8")
        if result.returncode == 0 or "Unable to load packaged ONNX Runtime" not in result.stderr:
            raise RuntimeError("Invalid custom DirectML.dll was not rejected by ORT loading")
        print("Invalid custom ORT sibling correctly rejected", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle", required=True, type=Path)
    parser.add_argument("--model-cache", required=True, type=Path)
    parser.add_argument("--logs", required=True, type=Path)
    args = parser.parse_args()
    check(args.bundle.resolve(), args.model_cache.resolve(), args.logs.resolve())
