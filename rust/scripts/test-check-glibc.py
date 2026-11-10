#!/usr/bin/env python3
"""Regression tests for the Linux runtime compatibility gate."""

from contextlib import redirect_stdout
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("check_glibc", Path(__file__).with_name("check-glibc.py"))
glibc = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(glibc)


class GlibcTests(unittest.TestCase):
    def test_weak_symbols_can_have_mandatory_version_requirements(self):
        symbols = """
  1: 0000000000000000     0 FUNC    GLOBAL DEFAULT  UND memcpy@GLIBC_2.14 (2)
  2: 0000000000000000     0 FUNC    GLOBAL DEFAULT  UND pthread_create@GLIBC_2.31 (3)
  3: 0000000000000000     0 FUNC    WEAK   DEFAULT  UND pidfd_spawnp@GLIBC_2.39 (4)
  4: 0000000000001234     8 FUNC    GLOBAL DEFAULT   12 exported@@GLIBC_2.40
  5: 0000000000000000     0 FUNC    GLOBAL DEFAULT  UND std_function@GLIBCXX_3.4.22 (5)
  6: 0000000000000000     0 FUNC    WEAK   DEFAULT  UND optional@GLIBC_2.41 (6)
Version definition section '.gnu.version_d' contains 1 entry:
  0x0010: Rev: 1 Flags: none Index: 2 Cnt: 1 Name: GLIBC_2.40
Version needs section '.gnu.version_r' contains 1 entry:
  0x0000: Version: 1 File: libc.so.6 Cnt: 2
  0x0010: Name: GLIBC_2.39 Flags: none Version: 4
  0x0020: Name: GLIBC_2.41 Flags: WEAK Version: 6
"""
        self.assertEqual(glibc.required_versions(symbols), {(2, 14), (2, 31), (2, 39)})

    def test_recursively_checks_elf_payloads_and_rejects_newer_imports(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            library = root / "lib" / "runtime.so.1"
            library.parent.mkdir()
            library.write_bytes(b"\x7fELF")
            (root / "LICENSE").write_text("license")
            with patch.object(glibc.subprocess, "check_output") as readelf, redirect_stdout(io.StringIO()):
                readelf.return_value = "1: 0 0 FUNC GLOBAL DEFAULT UND memcpy@GLIBC_2.27 (2)"
                glibc.audit(root, (2, 31), "custom-readelf")
                self.assertEqual(readelf.call_args.args[0],
                    ["custom-readelf", "--dyn-syms", "--version-info", "--wide", str(library)])
                self.assertEqual(readelf.call_args.kwargs["env"]["LC_ALL"], "C")
                readelf.return_value = """
1: 0 0 FUNC WEAK DEFAULT UND pidfd_spawnp@GLIBC_2.39 (2)
Version needs section '.gnu.version_r' contains 1 entry:
  0x0010: Name: GLIBC_2.39 Flags: none Version: 2
"""
                with self.assertRaisesRegex(RuntimeError, "baseline exceeded"):
                    glibc.audit(root, (2, 38))

    def test_unknown_mandatory_glibc_abi_cannot_silently_pass(self):
        with self.assertRaisesRegex(RuntimeError, "GLIBC_ABI_DT_RELR"):
            glibc.required_versions("""
Version needs section '.gnu.version_r' contains 1 entry:
  0x0010: Name: GLIBC_ABI_DT_RELR Flags: none Version: 2
""")

    def test_missing_payload_cannot_silently_pass(self):
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaisesRegex(RuntimeError, "No ELF payloads"):
                glibc.audit(Path(temporary), (2, 31))


if __name__ == "__main__":
    unittest.main()
