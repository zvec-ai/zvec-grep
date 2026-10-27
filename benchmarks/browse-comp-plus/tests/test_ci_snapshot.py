from __future__ import annotations

import io
import json
import os
import shutil
import tarfile
import tempfile
import unittest
from contextlib import ExitStack
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

import pyarrow as arrow
from pyarrow import parquet

from zg_bench.ci_snapshot import (
    STATE_FILES,
    cache_key,
    read_tar,
    verify_snapshot,
    write_tar,
)
from zg_bench.config import load_config
from zg_bench.corpus import materialize, prepared_corpus


class PreparedSnapshotTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.artifacts = self.root / "artifacts"
        self.key = "test-prepared-identity"

    def write(self, relative, data=b"fixture"):
        path = self.artifacts / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return path

    def fixture(self):
        self.write("source/queries.jsonl", b'{"query_id":"628"}\n')
        self.write("workspaces/a/document.md", b"corpus text\n")
        self.write("workspaces/b/document.md", b"corpus text\n")
        self.write("workspaces/b/.zvec-grep/index.bin", bytes(range(256)))
        self.write("models/potion/model.safetensors", b"local model weights")
        for name in STATE_FILES:
            self.write(f"state/{name}", b"{}\n")

    def pack(self, secrets=frozenset()):
        buffer = io.BytesIO()
        write_tar(self.artifacts, buffer, self.key, set(secrets))
        buffer.seek(0)
        return buffer

    def archive(self, entries=(), *, metadata=None, include_state=True):
        """Build an untrusted transport without write_tar's safety checks."""
        buffer = io.BytesIO()
        with tarfile.open(
            fileobj=buffer, mode="w", format=tarfile.PAX_FORMAT
        ) as archive:
            if metadata is None:
                metadata = {"key": self.key, "root": str(self.artifacts.resolve())}
            header = json.dumps(metadata).encode()
            info = tarfile.TarInfo("snapshot.json")
            info.size = len(header)
            archive.addfile(info, io.BytesIO(header))
            if include_state:
                entries = [
                    *(
                        (f"state/{name}", b"{}", tarfile.REGTYPE)
                        for name in STATE_FILES
                    ),
                    ("models/potion/model.safetensors", b"weights", tarfile.REGTYPE),
                    *entries,
                ]
            for name, data, kind in entries:
                info = tarfile.TarInfo(name)
                info.type = kind
                info.size = len(data)
                info.pax_headers["mtime"] = "1760000000.123456789"
                if kind in (tarfile.SYMTYPE, tarfile.LNKTYPE):
                    info.linkname = str(self.root / "outside")
                archive.addfile(info, io.BytesIO(data))
        buffer.seek(0)
        return buffer

    def test_roundtrip_preserves_contents_modes_and_exact_nanosecond_mtimes(self):
        self.fixture()
        before = {}
        for index, path in enumerate(sorted(self.artifacts.rglob("*"))):
            if path.is_file():
                timestamp = 1760000000123456789 + index
                path.chmod(0o640)
                os.utime(path, ns=(timestamp, timestamp))
                self.assertEqual(path.stat().st_mtime_ns, timestamp)
                before[path.relative_to(self.artifacts)] = (
                    path.read_bytes(),
                    path.stat().st_mtime_ns,
                    path.stat().st_mode & 0o777,
                )
        buffer = self.pack()
        shutil.rmtree(self.artifacts)
        read_tar(self.artifacts, buffer, self.key)
        after = {
            path.relative_to(self.artifacts): (
                path.read_bytes(),
                path.stat().st_mtime_ns,
                path.stat().st_mode & 0o777,
            )
            for path in self.artifacts.rglob("*")
            if path.is_file()
        }
        self.assertEqual(after, before)

    def test_restored_corpus_passes_preparation_validation_without_materialization(
        self,
    ):
        config = load_config()
        config = replace(
            config, dataset=replace(config.dataset, expected_corpus_documents=1)
        )
        source = self.artifacts / "source/corpus/fixture.parquet"
        source.parent.mkdir(parents=True)
        parquet.write_table(
            arrow.table(
                {
                    "docid": ["doc-1"],
                    "text": ["Corpus text"],
                    "url": ["https://example.test"],
                }
            ),
            source,
        )
        materialize(config, self.artifacts)
        timestamp = 1760000000123456789
        for profile in ("a", "b"):
            os.utime(
                self.artifacts / f"workspaces/{profile}/doc-1.md",
                ns=(timestamp, timestamp),
            )
        materialize(config, self.artifacts)
        for name in ("dataset.json", "index.json"):
            self.write(f"state/{name}", b"{}")
        self.write("workspaces/b/.zvec-grep/index.bin", b"index")
        self.write("models/potion/model.safetensors", b"local model weights")
        self.assertIsNotNone(prepared_corpus(config, self.artifacts))
        buffer = self.pack()
        shutil.rmtree(self.artifacts)
        read_tar(self.artifacts, buffer, self.key)
        self.assertEqual(
            prepared_corpus(config, self.artifacts),
            self.artifacts / "state/corpus.json",
        )
        self.assertTrue(
            (self.artifacts / "workspaces/b/.zvec-grep/index.bin").is_file()
        )

    def test_pack_excludes_runtime_credentials_and_run_profiles(self):
        self.fixture()
        self.write("runtime/zvec-home/auth.json", b"private-runtime")
        self.write("runs/run-1/profiles/baseline/auth.json", b"private-profile")
        self.write("state/runtime.json", b"private-runtime-state")
        buffer = self.pack({b"private-runtime", b"private-profile"})
        with tarfile.open(fileobj=buffer, mode="r:") as archive:
            names = archive.getnames()
        self.assertNotIn("state/runtime.json", names)
        self.assertFalse(any(name.startswith(("runtime/", "runs/")) for name in names))

    def test_restore_rejects_wrong_key_or_absolute_root_before_extracting(self):
        for metadata in (
            {"key": "wrong-candidate", "root": str(self.artifacts.resolve())},
            {"key": self.key, "root": str(self.root / "other-artifacts")},
        ):
            with self.subTest(metadata=metadata):
                with self.assertRaisesRegex(ValueError, "identity mismatch"):
                    read_tar(self.artifacts, self.archive(metadata=metadata), self.key)
                self.assertEqual(list(self.artifacts.iterdir()), [])

    def test_restore_rejects_unsafe_paths_and_nonregular_entries(self):
        entries = (
            ("../outside", b"overwrite", tarfile.REGTYPE),
            ("source/../../outside", b"overwrite", tarfile.REGTYPE),
            (str(self.root / "outside"), b"overwrite", tarfile.REGTYPE),
            ("runtime/auth.json", b"credential", tarfile.REGTYPE),
            ("state/runtime.json", b"credential", tarfile.REGTYPE),
            ("source/symlink", b"", tarfile.SYMTYPE),
            ("source/hardlink", b"", tarfile.LNKTYPE),
            ("source/device", b"", tarfile.CHRTYPE),
        )
        outside = self.root / "outside"
        outside.write_bytes(b"preserve")
        for entry in entries:
            with self.subTest(entry=entry):
                shutil.rmtree(self.artifacts, ignore_errors=True)
                with self.assertRaisesRegex(ValueError, "unsafe or duplicate"):
                    read_tar(self.artifacts, self.archive([entry]), self.key)
                self.assertEqual(outside.read_bytes(), b"preserve")

    def test_restore_rejects_duplicate_entries(self):
        entries = [("source/queries.jsonl", b"first", tarfile.REGTYPE)] * 2
        with self.assertRaisesRegex(ValueError, "unsafe or duplicate"):
            read_tar(self.artifacts, self.archive(entries), self.key)

    def test_restore_rejects_incomplete_snapshot(self):
        with self.assertRaisesRegex(ValueError, "incomplete"):
            read_tar(self.artifacts, self.archive(include_state=False), self.key)

    def test_restore_will_not_overwrite_existing_artifacts(self):
        existing = self.write("source/queries.jsonl", b"preserve")
        with self.assertRaisesRegex(ValueError, "empty artifacts directory"):
            read_tar(self.artifacts, self.archive(), self.key)
        self.assertEqual(existing.read_bytes(), b"preserve")

    def test_pack_rejects_secret_material_in_corpus_index_and_state(self):
        for relative in (
            "source/queries.jsonl",
            "workspaces/b/.zvec-grep/index.bin",
            "state/index.json",
            "models/potion/model.safetensors",
        ):
            with self.subTest(relative=relative):
                self.fixture()
                self.write(relative, b"prefix-provider-secret-suffix")
                with self.assertRaisesRegex(ValueError, "credential-bearing"):
                    self.pack({b"provider-secret"})

    def test_pack_rejects_auth_filename_even_when_secret_is_unknown(self):
        self.fixture()
        self.write("source/auth.json", b"unknown-credential")
        with self.assertRaisesRegex(ValueError, "credential-bearing"):
            self.pack()

    def test_pack_rejects_symlinks(self):
        self.fixture()
        outside = self.root / "outside"
        outside.write_bytes(b"private")
        (self.artifacts / "workspaces/a/linked.md").symlink_to(outside)
        with self.assertRaisesRegex(ValueError, "symlinks"):
            self.pack()

    def test_local_cache_identity_changes_with_candidate_or_root_not_remote_endpoint(
        self,
    ):
        initial = cache_key("candidate-a", "https://embedding-a.test", self.artifacts)
        self.assertEqual(
            initial,
            cache_key("candidate-a", "https://embedding-a.test", self.artifacts),
        )
        for candidate, endpoint, artifacts in (
            ("candidate-b", "https://embedding-a.test", self.artifacts),
            ("candidate-a", "https://embedding-a.test", self.root / "other-artifacts"),
        ):
            with self.subTest(
                candidate=candidate, endpoint=endpoint, artifacts=artifacts
            ):
                self.assertNotEqual(initial, cache_key(candidate, endpoint, artifacts))
        self.assertEqual(initial, cache_key("candidate-a", "", self.artifacts))
        self.assertEqual(
            initial,
            cache_key("candidate-a", "https://embedding-b.test", self.artifacts),
        )

    def test_remote_cache_identity_changes_with_endpoint_and_embedding(self):
        config = load_config()
        local_key = cache_key("candidate-a", "", self.artifacts)
        remote = replace(
            config,
            zvec_grep=replace(
                config.zvec_grep, embedding="qwen/qwen3.7-text-embedding"
            ),
        )
        with patch("zg_bench.ci_snapshot.load_config", return_value=remote):
            remote_key = cache_key(
                "candidate-a", "https://embedding-a.test", self.artifacts
            )
            self.assertNotEqual(local_key, remote_key)
            self.assertNotEqual(
                remote_key,
                cache_key("candidate-a", "https://embedding-b.test", self.artifacts),
            )

    def test_local_snapshot_requires_model_weights(self):
        self.fixture()
        shutil.rmtree(self.artifacts / "models")
        with self.assertRaisesRegex(ValueError, "input is missing"):
            self.pack()
        buffer = self.archive()
        with tarfile.open(fileobj=buffer, mode="r:") as archive:
            entries = [
                (member.name, archive.extractfile(member).read(), member.type)
                for member in archive
                if member.name.startswith("state/")
            ]
        shutil.rmtree(self.artifacts)
        with self.assertRaisesRegex(ValueError, "incomplete"):
            read_tar(
                self.artifacts, self.archive(entries, include_state=False), self.key
            )

    def test_verify_accepts_only_complete_snapshots_without_download_or_rebuild(self):
        checks = (
            "zg_bench.dataset.prepared_dataset",
            "zg_bench.corpus.prepared_corpus",
            "zg_bench.index.index_is_ready",
        )
        mutations = (
            "zg_bench.dataset.fetch",
            "zg_bench.corpus.materialize",
            "zg_bench.index.build_index",
        )
        for failed_check in (None, *checks):
            with self.subTest(failed_check=failed_check), ExitStack() as stack:
                validators = {
                    name: stack.enter_context(
                        patch(name, return_value=name != failed_check)
                    )
                    for name in checks
                }
                builders = [
                    stack.enter_context(
                        patch(name, side_effect=AssertionError("must not rebuild"))
                    )
                    for name in mutations
                ]
                if failed_check is None:
                    verify_snapshot(self.artifacts)
                    for validator in validators.values():
                        validator.assert_called_once_with(load_config(), self.artifacts)
                else:
                    with self.assertRaisesRegex(
                        ValueError, "remove its Actions cache and retry"
                    ):
                        verify_snapshot(self.artifacts)
                for builder in builders:
                    builder.assert_not_called()

    def test_verify_validation_errors_cannot_fall_back_to_paid_preparation(self):
        checks = (
            "zg_bench.dataset.prepared_dataset",
            "zg_bench.corpus.prepared_corpus",
            "zg_bench.index.index_is_ready",
        )
        for failed_check in checks:
            with self.subTest(failed_check=failed_check), ExitStack() as stack:
                for name in checks:
                    stack.enter_context(
                        patch(
                            name,
                            return_value=True,
                            side_effect=RuntimeError("invalid cached state")
                            if name == failed_check
                            else None,
                        )
                    )
                builders = [
                    stack.enter_context(
                        patch(name, side_effect=AssertionError("must not rebuild"))
                    )
                    for name in (
                        "zg_bench.dataset.fetch",
                        "zg_bench.corpus.materialize",
                        "zg_bench.index.build_index",
                    )
                ]
                with self.assertRaisesRegex(RuntimeError, "invalid cached state"):
                    verify_snapshot(self.artifacts)
                for builder in builders:
                    builder.assert_not_called()


if __name__ == "__main__":
    unittest.main()
