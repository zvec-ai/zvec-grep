#!/usr/bin/env python3
"""Load a deployed native ORT core and verify its version and C API."""

from contextlib import ExitStack
import ctypes
import os
from pathlib import Path
import sys


def check(path):
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
        if version != "1.21.0" or not base.get_api(21):
            raise RuntimeError(f"Expected ORT 1.21.0 with API 21; loaded {version}")
        print(f"Loaded ORT {version}, API 21: {path}")


if __name__ == "__main__":
    check(Path(sys.argv[1]))
