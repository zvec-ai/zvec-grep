import hashlib
import importlib.util
import io
import json
import lzma
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest import mock


spec = importlib.util.spec_from_file_location(
    "prepare_ort", Path(__file__).with_name("prepare-onnxruntime-windows.py")
)
prepare_ort = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare_ort)


class VerifiedRuntimeTests(unittest.TestCase):
    def test_selects_the_locked_directml_distribution_for_each_windows_architecture(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifests = root / "build/download"
            manifests.mkdir(parents=True)
            rows = ["target\tfeature_set\turl\tsha256_hash"]
            for index, target in enumerate(prepare_ort.TARGETS):
                rows.append(f"{target}\tdirectml\thttps://example.invalid/{target}\t{str(index) * 64}")
                rows.append(f"{target}\twebgpu\thttps://example.invalid/wrong-feature\t{'f' * 64}")
            (manifests / "dist.tsv").write_text("\n".join(rows), encoding="utf-8")
            metadata = {"packages": [{"name": "ort-sys", "manifest_path": str(root / "Cargo.toml")}]}
            with mock.patch.object(prepare_ort.subprocess, "check_output", return_value=json.dumps(metadata)) as cargo:
                for index, target in enumerate(prepare_ort.TARGETS):
                    with self.subTest(target=target):
                        dist = prepare_ort.distribution(root, target)
                        self.assertEqual(dist["url"], f"https://example.invalid/{target}")
                        self.assertEqual(dist["sha256_hash"], str(index) * 64)
                        self.assertEqual(cargo.call_args.args[0][-2:], ["--filter-platform", target])

    def test_raw_lzma2_runtime_is_extracted_into_the_cache(self):
        tar_bytes = io.BytesIO()
        with tarfile.open(fileobj=tar_bytes, mode="w") as tar:
            for name in ("onnxruntime.lib", "DirectML.dll"):
                entry = tarfile.TarInfo(name)
                entry.size = 7
                tar.addfile(entry, io.BytesIO(b"runtime"))
        compressed = lzma.compress(
            tar_bytes.getvalue(), format=lzma.FORMAT_RAW,
            filters=[{"id": lzma.FILTER_LZMA2, "dict_size": 1 << 26}],
        )
        expected = hashlib.sha256(compressed).hexdigest()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "runtime.tar.lzma2"
            archive.write_bytes(compressed)
            destination = root / "cache" / expected
            prepare_ort.extract_verified(archive, destination, expected)
            self.assertEqual((destination / "DirectML.dll").read_bytes(), b"runtime")
            self.assertEqual((destination / ".verified-sha256").read_text(), expected)

    def test_checksum_mismatch_never_creates_a_runtime_cache(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "runtime.tar.lzma2"
            archive.write_bytes(b"truncated download")
            destination = root / "cache"
            with self.assertRaisesRegex(RuntimeError, "SHA-256"):
                prepare_ort.extract_verified(archive, destination, "0" * 64)
            self.assertFalse(destination.exists())


if __name__ == "__main__":
    unittest.main()
