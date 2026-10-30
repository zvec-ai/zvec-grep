from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from zg_bench.artifacts import read_json, write_json
from zg_bench.config import load_config
from zg_bench.index import build_index, index_is_ready
from zg_bench.process import CommandResult
from zg_bench.profiles import server_environment


class LocalEmbeddingTests(unittest.TestCase):
    def test_local_index_and_restored_search_share_model_cache_without_remote_credentials(
        self,
    ):
        with tempfile.TemporaryDirectory() as directory:
            artifacts = Path(directory).resolve() / "artifacts"
            workspace = artifacts / "workspaces/b"
            workspace.mkdir(parents=True)
            write_json(artifacts / "state/corpus.json", {"fingerprint": "corpus"})
            config = load_config()
            commands = []

            def run(args, **kwargs):
                commands.append(list(map(str, args)))
                if args[1] == "--version":
                    return CommandResult(
                        tuple(map(str, args)), 0, "fixture-version", ""
                    )
                environment = kwargs["env"]
                self.assertNotIn("ZVEC_GREP_API_KEY", environment)
                self.assertNotIn("ZVEC_GREP_ENDPOINT", environment)
                self.assertEqual(
                    environment["ZVEC_GREP_MODEL_CACHE"], str(artifacts / "models")
                )
                if args[1] == "--index":
                    self.assertEqual(
                        args[args.index("--embedding") + 1],
                        "local/potion-multilingual-128m",
                    )
                    self.assertEqual(args[args.index("--device") + 1], "cpu")
                    self.assertEqual(
                        args[args.index("--index-embedding-concurrency") + 1], "4"
                    )
                    (workspace / ".zvec-grep").mkdir()
                    model = artifacts / "models/potion/model.safetensors"
                    model.parent.mkdir(parents=True)
                    model.write_bytes(b"fixture weights")
                return CommandResult(tuple(map(str, args)), 0, "ready", "")

            with (
                patch.dict(os.environ, {}, clear=True),
                patch(
                    "zg_bench.index.resolve_executable",
                    return_value=Path("/fixture/zg"),
                ),
                patch("zg_bench.index.run_command", side_effect=run),
                patch("zg_bench.index.run_streaming_command", side_effect=run),
            ):
                state_path = build_index(config, artifacts)
                self.assertTrue(index_is_ready(config, artifacts))
                self.assertEqual(build_index(config, artifacts), state_path)
                environment = server_environment(config, artifacts)
            self.assertFalse(any("--auth" in command for command in commands))
            self.assertEqual(sum("--index" in command for command in commands), 1)
            self.assertEqual(
                read_json(state_path)["model_cache_bytes"], len(b"fixture weights")
            )
            self.assertEqual(
                environment["ZVEC_GREP_MODEL_CACHE"], str(artifacts / "models")
            )


if __name__ == "__main__":
    unittest.main()
