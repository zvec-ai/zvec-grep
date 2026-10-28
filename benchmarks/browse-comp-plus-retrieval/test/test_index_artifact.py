import importlib.util
import io
import json
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

MODULE = Path(__file__).resolve().parents[1] / "index_artifact.py"
SPEC = importlib.util.spec_from_file_location("index_artifact", MODULE)
artifact = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(artifact)


class IndexArtifactTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.key = "a" * 64
        for directory in ["index", "model", "evidence"]:
            (self.root / directory).mkdir()
        (self.root / "index/data").write_bytes(b"index")
        (self.root / "model/weights").write_bytes(b"model")
        (self.root / "evidence/index.log").write_text("original build evidence")
        build = {
            "identity": {"key": self.key},
            "status": "success",
            "document_count": 100195,
            "status_snapshot": {"files": 100195},
            "arm": {
                "performance_groups": [
                    {
                        "index": {
                            "status": "success",
                            "memory": {"status": "available"},
                            "index_bytes": 5,
                        }
                    }
                ]
            },
        }
        (self.root / "report.json").write_text(json.dumps(build))

    def pack(self):
        return artifact.pack(
            *(
                self.root / name
                for name in ["index", "model", "evidence", "report.json", "sealed"]
            )
        )

    def test_round_trip_keeps_exact_index_model_and_builder_evidence(self):
        original = self.pack()
        restored = artifact.verify(
            self.root / "sealed", self.key, self.root / "restored"
        )
        self.assertEqual(original, restored)
        self.assertEqual((self.root / "restored/index/data").read_bytes(), b"index")
        self.assertEqual((self.root / "restored/model/weights").read_bytes(), b"model")
        self.assertTrue((self.root / "restored/build/index.log").is_file())

    def test_wrong_storage_or_corrupt_archive_is_rejected(self):
        self.pack()
        with self.assertRaises(ValueError):
            artifact.verify(self.root / "sealed", "b" * 64)
        with (self.root / "sealed/index.tar.gz").open("ab") as destination:
            destination.write(b"corruption")
        with self.assertRaisesRegex(ValueError, "checksum"):
            artifact.verify(self.root / "sealed", self.key)

    def test_partial_index_or_failed_measurements_cannot_be_published(self):
        path = self.root / "report.json"
        build = json.loads(path.read_text())
        build["document_count"] = 10000
        path.write_text(json.dumps(build))
        with self.assertRaisesRegex(ValueError, "complete"):
            self.pack()

    def test_unsafe_tar_names_links_and_duplicate_files_are_rejected(self):
        self.pack()
        for name, kind in [
            ("index/../../escape", tarfile.REGTYPE),
            ("index/data", tarfile.SYMTYPE),
        ]:
            path = self.root / "sealed/index.tar.gz"
            with tarfile.open(path, "w:gz") as target:
                info = tarfile.TarInfo(name)
                info.type = kind
                info.size = 5 if kind == tarfile.REGTYPE else 0
                info.linkname = "/tmp/escape" if kind == tarfile.SYMTYPE else ""
                target.addfile(info, io.BytesIO(b"index") if info.size else None)
            manifest_path = self.root / "sealed/manifest.json"
            manifest = json.loads(manifest_path.read_text())
            manifest["archive_sha256"] = artifact.digest(path)
            manifest_path.write_text(json.dumps(manifest))
            with self.assertRaises(ValueError):
                artifact.verify(self.root / "sealed", self.key)

    def test_zip_wrapper_must_contain_only_the_two_sealed_files(self):
        path = self.root / "artifact.zip"
        with zipfile.ZipFile(path, "w") as target:
            target.writestr("../../escape", "bad")
        with self.assertRaises(ValueError):
            artifact.unzip(path, self.root / "unpacked")


if __name__ == "__main__":
    unittest.main()
