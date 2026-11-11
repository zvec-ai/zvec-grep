#!/usr/bin/env python3
"""Offline static SDK cache and linkage contract tests; no source downloads."""

import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from contextlib import redirect_stdout
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("pdfium", Path(__file__).with_name("prepare-pdfium.py"))
pdfium = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(pdfium)


class StaticPdfiumTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.manifest = json.loads(pdfium.MANIFEST_PATH.read_text())

    def sdk(self, target):
        root = self.root / self.manifest["version"] / target / "sdk"
        (root / "lib").mkdir(parents=True)
        (root / "licenses").mkdir()
        names = ["pdfium.lib"] if "windows" in target else ["libpdfium.a"]
        if "linux" in target:
            names += (["libstdc++.a"] if self.manifest["targets"][target].get("compiler") == "gcc"
                      else ["libc++.a", "libc++abi.a"])
        for name in names:
            (root / "lib" / name).write_bytes(b"!<arch>\n")
        (root / "licenses/pdfium-ThirdPartyNotices.txt").write_text("PDFium license")
        state = {"identity": pdfium.identity(self.manifest, target),
                 "files": {p.relative_to(root).as_posix(): pdfium.digest(p)
                           for p in root.rglob("*") if p.is_file()}}
        (root / pdfium.MARKER).write_text(json.dumps(state))
        return root

    def test_offline_reuse_emits_static_links_and_retains_notices_for_every_target(self):
        for target in self.manifest["targets"]:
            with self.subTest(target=target):
                sdk = self.sdk(target)
                profile = self.root / "cargo" / target
                with patch.object(pdfium, "build_sdk", side_effect=AssertionError("must reuse SDK")), redirect_stdout(io.StringIO()) as output:
                    self.assertEqual(pdfium.prepare(self.root, target, profile, True), sdk)
                flags = output.getvalue()
                self.assertIn("cargo:rustc-link-lib=static=pdfium\n", flags)
                self.assertNotIn("dylib=pdfium", flags)
                self.assertEqual((profile / "licenses/pdfium-ThirdPartyNotices.txt").read_text(), "PDFium license")
                self.assertFalse(list(profile.glob("*.dll")) + list(profile.glob("*.dylib")) + list(profile.glob("*.so")))
                if "linux" in target and self.manifest["targets"][target].get("compiler") != "gcc":
                    self.assertIn("cargo:rustc-link-lib=static=c++abi", flags)

    def test_offline_missing_corrupt_or_changed_revision_never_downloads(self):
        target = "aarch64-apple-darwin"
        with patch.object(pdfium, "build_sdk", side_effect=AssertionError("must remain offline")):
            with self.assertRaisesRegex(RuntimeError, "Offline build requires"):
                pdfium.prepare(self.root, target, offline=True)
            sdk = self.sdk(target)
            (sdk / "lib/libpdfium.a").write_bytes(b"corrupt")
            with self.assertRaisesRegex(RuntimeError, "Offline build requires"):
                pdfium.prepare(self.root, target, offline=True)
            (sdk / "lib/libpdfium.a").write_bytes(b"!<arch>\n")
            state = json.loads((sdk / pdfium.MARKER).read_text())
            state["identity"]["revision"] = "wrong"
            (sdk / pdfium.MARKER).write_text(json.dumps(state))
            with self.assertRaisesRegex(RuntimeError, "Offline build requires"):
                pdfium.prepare(self.root, target, offline=True)

    def test_checksums_do_not_make_thin_or_shared_library_a_valid_sdk(self):
        target = "x86_64-unknown-linux-gnu"
        sdk = self.sdk(target)
        for payload in [b"!<thin>\n", b"\x7fELFshared"]:
            (sdk / "lib/libpdfium.a").write_bytes(payload)
            state = json.loads((sdk / pdfium.MARKER).read_text())
            state["files"]["lib/libpdfium.a"] = pdfium.digest(sdk / "lib/libpdfium.a")
            (sdk / pdfium.MARKER).write_text(json.dumps(state))
            self.assertFalse(pdfium.verified_sdk(sdk, pdfium.identity(self.manifest, target)))
        (sdk / "lib/libc++abi.a").unlink()
        self.assertFalse(pdfium.verified_sdk(sdk, pdfium.identity(self.manifest, target)))

    def test_platform_build_flags_preserve_native_runtime_contract(self):
        linux = pdfium.gn_arguments({"os": "linux", "cpu": "x64"})
        self.assertFalse(linux["use_sysroot"])
        self.assertTrue(linux["use_custom_libcxx"])
        arm = pdfium.gn_arguments(self.manifest["targets"]["aarch64-unknown-linux-gnu"])
        self.assertFalse(arm["is_clang"])
        self.assertFalse(arm["use_custom_libcxx"])
        with patch.dict(os.environ, {"MACOSX_DEPLOYMENT_TARGET": "14.0"}):
            mac = pdfium.gn_arguments({"os": "mac", "cpu": "x64"})
            self.assertEqual(mac["mac_deployment_target"], "14.0")
        source = self.root / "source"
        config = source / "build/config/win/BUILD.gn"
        config.parent.mkdir(parents=True)
        config.write_text('configs = [ ":static_crt" ]\n')
        pdfium.platform_toolchain(source, {}, "win")
        self.assertEqual(config.read_text(), 'configs = [ ":dynamic_crt" ]\n')
        pdfium.platform_toolchain(source, {}, "win")
        self.assertEqual(config.read_text(), 'configs = [ ":dynamic_crt" ]\n')

    @unittest.skipIf(os.name == "nt", "macOS SDK discovery uses Unix symlinks")
    def test_command_line_tools_sdk_does_not_require_xcodebuild(self):
        import plistlib
        source = self.root / "pdfium"
        config = source / "build/config/mac/mac_sdk.gni"
        config.parent.mkdir(parents=True)
        developer = self.root / "CommandLineTools"
        (developer / "SDKs/MacOSX.sdk").mkdir(parents=True)
        (developer / "usr/bin").mkdir(parents=True)
        pristine = '# SDK settings\n# The path to the hermetic install of Xcode.\nold discovery\n_sdk_root = rebase_path(mac_sdk_path, root_build_dir)\n'
        responses = [pristine, str(developer), str(developer / "SDKs/MacOSX.sdk"), "26.5", "25F70"]
        with patch.object(pdfium, "run", side_effect=responses) as commands, patch.object(
                pdfium.subprocess, "check_output", return_value=plistlib.dumps({"pkg-version": "26.6.0.0.1"})):
            pdfium.platform_toolchain(source, {}, "mac")
        text = config.read_text()
        self.assertNotIn("old discovery", text)
        self.assertIn('mac_sdk_path = "//out/zg-static/sdk/CommandLineTools.sdk"', text)
        self.assertIn('xcode_version_int = 2660', text)
        self.assertEqual((source / "out/zg-static/sdk/CommandLineTools.sdk").resolve(), developer / "SDKs/MacOSX.sdk")
        self.assertTrue(all(call.args[0][0] != "xcodebuild" for call in commands.call_args_list))

    def test_dependency_filter_retains_pinned_compiler_and_library_sources(self):
        source = "deps = {'third_party/rust-toolchain': {'bucket': 'x', 'objects': [1]}, 'third_party/llvm-build/Release+Asserts': {'objects': [2]}, 'third_party/freetype': 'repo@commit', 'third_party/siso/cipd': {'packages': [3]}}"
        filtered = pdfium.library_deps(source)
        import ast
        deps = ast.literal_eval(ast.parse(filtered).body[0].value)
        self.assertIsNone(deps['third_party/rust-toolchain'])
        self.assertIsNone(deps['third_party/siso/cipd'])
        self.assertEqual(deps['third_party/llvm-build/Release+Asserts'], {'objects': [2]})
        self.assertEqual(deps['third_party/freetype'], 'repo@commit')


if __name__ == "__main__":
    unittest.main()
