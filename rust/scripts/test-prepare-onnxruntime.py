#!/usr/bin/env python3
"""Offline tests for the shared, multi-platform ORT deployment contract."""

import copy
from contextlib import redirect_stdout
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile

SCRIPT = Path(__file__).with_name("prepare-onnxruntime.py")
SPEC = importlib.util.spec_from_file_location("prepare_ort", SCRIPT)
ort = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ort)
MANIFEST = json.loads(ort.MANIFEST_PATH.read_text())


def make_archive(path, names):
    if path.suffix == ".tgz":
        with tarfile.open(path, "w:gz") as archive:
            for name in names:
                data = name.encode()
                entry = tarfile.TarInfo(name)
                entry.size = len(data)
                entry.mtime = 1700000000
                archive.addfile(entry, io.BytesIO(data))
    else:
        with zipfile.ZipFile(path, "w") as archive:
            for name in names:
                archive.writestr(zipfile.ZipInfo(name, (2023, 11, 14, 0, 0, 0)), name.encode())


class RuntimeTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.cache = self.root / "cache"
        self.cache.mkdir()
        self.manifest = copy.deepcopy(MANIFEST)
        # NuGet archives contain multiple architectures. Match their real layout
        # so tests catch selecting a DLL from the wrong architecture.
        archives = {}
        for artifact in self.manifest["artifacts"].values():
            archives.setdefault(artifact["archive"], set()).update(artifact["files"])
        for name, names in archives.items():
            make_archive(self.cache / name, [*sorted(names), "../../unwanted-file",
                                           "symbols/libonnxruntime.1.21.0.dylib"])
        for artifact in self.manifest["artifacts"].values():
            artifact["sha256"] = ort.digest(self.cache / artifact["archive"])

    def stage(self, target="x86_64-unknown-linux-gnu", cuda=False, destination=None):
        with redirect_stdout(io.StringIO()), patch.object(ort.subprocess, "run", side_effect=AssertionError("unexpected network")):
            return ort.prepare(self.cache, destination or self.root / "release", target,
                               cuda=cuda, cargo=True, offline=True, manifest=self.manifest)

    def test_every_target_and_variant_is_relocatable_and_repairs_outputs(self):
        for target, variants in self.manifest["targets"].items():
            for variant in variants:
                with self.subTest(target=target, variant=variant):
                    profile = self.root / target / variant / "release"
                    outputs = self.stage(target, variant == "cuda", profile)
                    wanted = {}
                    for artifact in ort.selection(self.manifest, target, variant == "cuda"):
                        wanted.update({destination: source.encode() for source, destination in artifact["files"].items()})
                    for directory in (profile, profile / "deps", profile / "examples"):
                        for name, content in wanted.items():
                            self.assertEqual((directory / name).read_bytes(), content)
                            self.assertFalse((directory / name).is_symlink())
                    mtimes = [p.stat().st_mtime_ns for p in outputs]
                    self.stage(target, variant == "cuda", profile)
                    self.assertEqual(mtimes, [p.stat().st_mtime_ns for p in outputs])
                    outputs[0].unlink()
                    outputs[1].write_bytes(b"corrupt deployed file")
                    unrelated = profile / "user-file"
                    unrelated.write_bytes(b"keep")
                    self.stage(target, variant == "cuda", profile)
                    self.assertEqual(mtimes, [p.stat().st_mtime_ns for p in outputs])
                    self.assertEqual(unrelated.read_bytes(), b"keep")
                    self.assertFalse((self.root / "unwanted-file").exists())
                    if os.name != "nt":
                        outputs[0].unlink()
                        outputs[0].symlink_to(unrelated)
                        self.stage(target, variant == "cuda", profile)
                        self.assertFalse(outputs[0].is_symlink())
                        self.assertEqual(unrelated.read_bytes(), b"keep")

    def test_invalid_archives_preserve_existing_deployment(self):
        outputs = self.stage()
        before = [p.read_bytes() for p in outputs]
        artifact = ort.selection(self.manifest, "x86_64-unknown-linux-gnu")[0]
        archive = self.cache / artifact["archive"]
        names = list(artifact["files"])
        for entries in (names[:-1], [*names, names[0]]):
            make_archive(archive, entries)
            artifact["sha256"] = ort.digest(archive)
            with self.assertRaises(RuntimeError):
                self.stage()
            self.assertEqual(before, [p.read_bytes() for p in outputs])
        archive.write_bytes(b"invalid cached archive")
        with self.assertRaisesRegex(RuntimeError, "Offline build requires"):
            self.stage()
        self.assertEqual(before, [p.read_bytes() for p in outputs])

    def test_cpu_cuda_switch_removes_only_owned_stale_provider_files(self):
        self.stage(cuda=True)
        profile = self.root / "release"
        provider = profile / "libonnxruntime_providers_cuda.so"
        self.assertTrue(provider.exists())
        provider.write_bytes(b"user modification")
        marker = (profile / ort.MARKER).read_bytes()
        with self.assertRaisesRegex(RuntimeError, "was modified"):
            self.stage()
        self.assertEqual((profile / ort.MARKER).read_bytes(), marker)
        self.stage(cuda=True)
        self.stage()
        self.assertFalse(provider.exists())
        self.assertFalse((profile / "deps" / provider.name).exists())
        self.assertTrue((profile / "libonnxruntime.so.1").is_file())

    def test_failed_download_does_not_replace_cache_or_runtime(self):
        outputs = self.stage()
        before = [p.read_bytes() for p in outputs]
        artifact = ort.selection(self.manifest, "x86_64-unknown-linux-gnu")[0]
        archive = self.cache / artifact["archive"]
        archive.write_bytes(b"old invalid cache")

        def download(command, **_):
            Path(command[command.index("-o") + 1]).write_bytes(b"wrong response")

        with patch.object(ort.subprocess, "run", side_effect=download):
            with self.assertRaisesRegex(RuntimeError, "SHA-256 mismatch"):
                ort.prepare(self.cache, self.root / "release", "x86_64-unknown-linux-gnu",
                            manifest=self.manifest)
        self.assertEqual(archive.read_bytes(), b"old invalid cache")
        self.assertEqual(before, [p.read_bytes() for p in outputs])

    def test_target_selection_and_unsafe_destinations_fail_before_deployment(self):
        with self.assertRaisesRegex(RuntimeError, "No official ORT"):
            self.stage("aarch64-unknown-linux-gnu", cuda=True)
        artifact = ort.selection(self.manifest, "x86_64-unknown-linux-gnu")[0]
        artifact["files"][next(iter(artifact["files"]))] = "../escape"
        with self.assertRaisesRegex(RuntimeError, "Unsafe runtime path"):
            self.stage()
        self.assertFalse((self.root / "escape").exists())

    def test_offline_cli_uses_cross_target_out_dir(self):
        out = self.root / "target/aarch64-unknown-linux-gnu/debug/build/zg-engine-hash/out"
        result = subprocess.run([sys.executable, str(SCRIPT), "--target", "aarch64-unknown-linux-gnu",
                                 "--cargo-out-dir", str(out)], capture_output=True, text=True,
                                env={**os.environ, "CARGO_NET_OFFLINE": "true"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Offline build requires", result.stderr)
        self.assertIn(str(out.parents[3] / "ort-prebuilt"), result.stderr)


if __name__ == "__main__":
    unittest.main()
