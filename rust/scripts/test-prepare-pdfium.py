#!/usr/bin/env python3
"""Isolated staging tests; no downloads, compilers or developer cache."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("pdfium", Path(__file__).with_name("prepare-pdfium.py"))
pdfium = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(pdfium)


class StagingTests(unittest.TestCase):
    def test_six_platforms_include_runtime_and_notices_without_static_archives(self):
        targets = ["aarch64-apple-darwin", "x86_64-apple-darwin", "aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu", "aarch64-pc-windows-msvc", "x86_64-pc-windows-msvc"]
        for target in targets:
            with self.subTest(target=target), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                sdk = root / "sdk"
                (sdk / "lib").mkdir(parents=True)
                (sdk / "bin").mkdir()
                (sdk / "licenses").mkdir()
                (sdk / "VERSION").write_text("MAJOR=155\nBUILD=8058\n")
                (sdk / "LICENSE").write_text("PDFium")
                (sdk / "licenses/notice.txt").write_bytes(b"Copyright \xa9")
                platform = "macos" if "apple" in target else "windows" if "windows" in target else "linux"
                name = pdfium.LIBRARIES[platform]
                source = sdk / ("bin" if platform == "windows" else "lib") / name
                source.write_bytes(b"test runtime")
                profile = root / "release"
                staged = pdfium.prepare(sdk / "lib", target, profile)
                self.assertEqual(staged.read_bytes(), b"test runtime")
                self.assertIn("Copyright ©", (profile / "licenses/pdfium-ThirdPartyNotices.txt").read_text())
                self.assertEqual(sorted(p.name for p in profile.iterdir()), sorted([name, "licenses"]))
                source.write_bytes(b"updated runtime")
                pdfium.prepare(sdk / "lib", target, profile)
                self.assertEqual(staged.read_bytes(), b"updated runtime")
                (sdk / "VERSION").write_text("BUILD=8076\n")
                with self.assertRaisesRegex(RuntimeError, "patched PDFium"):
                    pdfium.prepare(sdk / "lib", target, profile)
                self.assertEqual(staged.read_bytes(), b"updated runtime")


if __name__ == "__main__":
    unittest.main()
