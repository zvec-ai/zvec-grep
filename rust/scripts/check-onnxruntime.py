#!/usr/bin/env python3
"""Load a deployed native ORT core and verify its version and C API."""

from contextlib import ExitStack
import argparse
import ctypes
import os
from pathlib import Path


def check(path, expected="1.21.0"):
    with ExitStack() as stack:
        if os.name == "nt":
            stack.enter_context(os.add_dll_directory(str(path.parent.resolve())))
        library = ctypes.CDLL(str(path.resolve()))

        class ApiBase(ctypes.Structure):
            _fields_ = [("get_api", ctypes.CFUNCTYPE(ctypes.c_void_p, ctypes.c_uint32)),
                        ("get_version", ctypes.CFUNCTYPE(ctypes.c_char_p))]

        library.OrtGetApiBase.restype = ctypes.POINTER(ApiBase)
        base = library.OrtGetApiBase().contents
        version = base.get_version().decode()
        if version != expected or not base.get_api(21):
            raise RuntimeError(f"Expected ORT {expected} with API 21; loaded {version}")
        print(f"Loaded ORT {version}, API 21: {path}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", type=Path)
    parser.add_argument("--expected", default="1.21.0")
    args = parser.parse_args()
    check(args.path, args.expected)
