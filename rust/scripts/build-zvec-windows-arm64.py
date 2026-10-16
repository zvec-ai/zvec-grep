#!/usr/bin/env python3
"""Build the zvec 0.7.2 native SDK on a Windows ARM64 runner."""

import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile


# This native revision is recorded by zvec-rust 0.7.2's vendor/zvec submodule.
ZVEC_REVISION = "1ab7975dfc2d2160054bafff614831b7099cd930"


def run(*args):
    subprocess.run([str(arg) for arg in args], check=True)


def main():
    if sys.platform != "win32" or platform.machine().lower() not in ("arm64", "aarch64"):
        raise RuntimeError("zvec source builds require a native Windows ARM64 runner")
    runtime = Path("target/zvec-runtime").resolve()
    with tempfile.TemporaryDirectory(prefix="zg-zvec-", dir=os.environ.get("RUNNER_TEMP")) as temporary:
        root = Path(temporary)
        source, build, tools = root / "source", root / "build", root / "tools"
        run("git", "init", source)
        run("git", "-C", source, "remote", "add", "origin", "https://github.com/alibaba/zvec.git")
        run("git", "-C", source, "fetch", "--depth", "1", "origin", ZVEC_REVISION)
        run("git", "-C", source, "checkout", "--detach", "FETCH_HEAD")
        run("git", "-C", source, "submodule", "update", "--init", "--recursive", "--depth", "1")
        # Match upstream's Windows build, using the runner's native MSVC/Ninja.
        run(sys.executable, "-m", "venv", tools)
        run(tools / "Scripts/python.exe", "-m", "pip", "install", "cmake==3.30.0")
        cmake = tools / "Scripts/cmake.exe"
        run(cmake, "-S", source, "-B", build, "-G", "Ninja",
            "-DCMAKE_BUILD_TYPE=Release", "-DBUILD_C_BINDINGS=ON",
            "-DBUILD_PYTHON_BINDINGS=OFF", "-DBUILD_TOOLS=OFF")
        run(cmake, "--build", build, "--config", "Release", "--parallel", "2", "--target", "zvec_c_api")
        runtime.mkdir(parents=True, exist_ok=True)
        for folder in ("bin", "lib"):
            for library in (build / folder).glob("*.dll"):
                shutil.copy2(library, runtime / library.name)
        shutil.copy2(build / "lib/zvec_c_api.lib", runtime / "zvec_c_api.lib")
        dictionaries = runtime / "data/jieba_dict"
        dictionaries.mkdir(parents=True, exist_ok=True)
        for name in ("jieba.dict.utf8", "hmm_model.utf8"):
            shutil.copy2(source / "thirdparty/cppjieba/cppjieba-5.6.7/dict" / name, dictionaries / name)
        if not (runtime / "zvec_c_api.dll").is_file():
            raise RuntimeError("zvec build did not produce zvec_c_api.dll")
    print(f"Built Windows ARM64 zvec SDK: {runtime}")


if __name__ == "__main__":
    main()
