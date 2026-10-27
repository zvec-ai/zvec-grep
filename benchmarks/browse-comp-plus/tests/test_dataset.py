from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from huggingface_hub.utils import build_hf_headers
from zg_bench.config import load_config
from zg_bench.dataset import fetch


class DatasetAuthenticationTests(unittest.TestCase):
    def test_unconfigured_optional_token_downloads_both_snapshots_anonymously(self):
        for token in ("", " \n\t"):
            with self.subTest(token=token), tempfile.TemporaryDirectory() as root:
                with (
                    patch("zg_bench.dataset.snapshot_download") as download,
                    self.assertRaisesRegex(RuntimeError, "Parquet"),
                ):
                    fetch(load_config(), Path(root), token=token)
                self.assertEqual(download.call_count, 2)
                for call in download.call_args_list:
                    self.assertIs(call.kwargs["token"], False)
                    self.assertNotIn(
                        "authorization", build_hf_headers(token=call.kwargs["token"])
                    )

    def test_configured_token_is_preserved_and_missing_argument_uses_hub_defaults(self):
        for token, expected in (("hf_fixture", "hf_fixture"), (None, None)):
            with self.subTest(token=token), tempfile.TemporaryDirectory() as root:
                with (
                    patch("zg_bench.dataset.snapshot_download") as download,
                    self.assertRaisesRegex(RuntimeError, "Parquet"),
                ):
                    fetch(load_config(), Path(root), token=token)
                self.assertEqual(download.call_count, 2)
                for call in download.call_args_list:
                    self.assertEqual(call.kwargs["token"], expected)
