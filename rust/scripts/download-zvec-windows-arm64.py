#!/usr/bin/env python3
"""Download the pinned upstream zvec SDK for Windows ARM64 packaging."""

import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile


# Match the pinned zvec-rust dependency and verify the upstream release asset.
VERSION = "0.8.0"
TARGET = "aarch64-pc-windows-msvc"
SHA256 = "f63381c6eae1d029bb1fbc58f2ad4aa073336b0b5adc35024d19acdc1653cf57"


def extract_verified(archive, destination):
    with archive.open("rb") as source:
        if hashlib.file_digest(source, "sha256").hexdigest() != SHA256:
            raise RuntimeError("zvec SDK archive SHA-256 differs from the pinned release")
    with tempfile.TemporaryDirectory() as temporary:
        sdk = Path(temporary)
        with tarfile.open(archive, "r:gz") as tar:
            tar.extractall(sdk, filter="data")
        if (sdk / "TARGET").read_text(encoding="ascii").strip() != TARGET:
            raise RuntimeError("zvec SDK archive has an unexpected target")
        libraries = ("zvec_c_api.dll", "zvec_c_api.lib")
        for name in libraries:
            if not (sdk / name).is_file():
                raise RuntimeError(f"zvec SDK archive is missing {name}")
        destination.mkdir(parents=True, exist_ok=True)
        for name in (*libraries, "TARGET"):
            shutil.copy2(sdk / name, destination / name)
    print(f"Verified prebuilt zvec SDK for {TARGET}: {destination}")


if __name__ == "__main__":
    with tempfile.TemporaryDirectory() as temporary:
        archive = Path(temporary) / f"zvec-prebuilt-{TARGET}.tar.gz"
        subprocess.run([
            "curl.exe" if os.name == "nt" else "curl",
            "--fail", "--location", "--retry", "3", "--output", str(archive),
            f"https://github.com/zvec-ai/zvec-rust/releases/download/v{VERSION}/{archive.name}",
        ], check=True)
        extract_verified(archive, Path("target/zvec-runtime").resolve())
