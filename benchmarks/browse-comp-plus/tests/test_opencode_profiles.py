from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from zg_bench.artifacts import read_json, write_json
from zg_bench.config import load_config
from zg_bench.process import CommandResult
from zg_bench.profiles import (
    model_base_url,
    opencode_environment,
    prepare_profiles,
    validate_profiles,
)


class OpenCodeProfileTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.config = load_config()
        environment = patch.dict(
            os.environ,
            {
                "OPENAI_API_KEY": "fixture-model-key",
                "BROWSE_COMP_MODEL_BASE_URL": "https://qwen.example.test/v1",
            },
        )
        environment.start()
        self.addCleanup(environment.stop)

    def test_profiles_install_treatment_and_validate_configuration_identity(self):
        manifest = self.root / "profiles.json"

        def install(args, **kwargs):
            self.assertIn("opencode", args)
            self.assertEqual(kwargs["env"]["OPENAI_API_KEY"], "fixture-model-key")
            self.assertNotIn("ZVEC_GREP_API_KEY", kwargs["env"])
            target = Path(kwargs["env"]["OPENCODE_CONFIG"])
            configuration = read_json(target)
            configuration["mcp"] = {
                "zvec_grep": {
                    "type": "remote",
                    "url": "http://127.0.0.1:7999/mcp",
                    "enabled": True,
                    "timeout": 600000,
                    "oauth": False,
                }
            }
            write_json(target, configuration)
            (target.parent / "AGENTS.md").write_text(
                "<!-- ZVEC_GREP_START -->\nfixture guidance"
            )
            return CommandResult(tuple(map(str, args)), 0, "installed", "")

        with (
            patch(
                "zg_bench.profiles.resolve_executable", return_value=self.root / "zg"
            ),
            patch(
                "zg_bench.profiles.zvec_grep_build_identity",
                return_value={"fingerprint": "candidate"},
            ),
            patch("zg_bench.profiles.run_command", side_effect=install),
        ):
            prepare_profiles(
                self.config,
                self.root / "artifacts",
                profiles_root=self.root / "profiles",
                manifest_path=manifest,
            )
            state = validate_profiles(manifest)
            baseline = read_json(Path(state["baseline_home"]) / "opencode.json")
            treatment_path = Path(state["treatment_home"]) / "opencode.json"
            treatment = read_json(treatment_path)
            self.assertNotIn("mcp", baseline)
            self.assertEqual(baseline["instructions"], [])
            self.assertEqual(
                treatment["instructions"], [str(treatment_path.parent / "AGENTS.md")]
            )
            self.assertEqual(baseline["provider"], treatment["provider"])
            self.assertNotIn("fixture-model-key", treatment_path.read_text())
            self.assertEqual(baseline["permission"]["task"], "deny")
            treatment["permission"]["webfetch"] = "allow"
            write_json(treatment_path, treatment)
            with self.assertRaisesRegex(RuntimeError, "profile files changed"):
                validate_profiles(manifest)

    def test_process_environment_excludes_user_config_and_embedding_credentials(self):
        with patch.dict(
            os.environ,
            {
                "OPENCODE_CONFIG_CONTENT": '{"permission":"allow"}',
                "OPENCODE_PERMISSION": "allow",
                "OPENCODE_CONFIG": "/unrelated/config.json",
                "CODEX_HOME": "/unrelated/codex",
                "ZVEC_GREP_API_KEY": "embedding-secret",
                "HF_TOKEN": "hf-secret",
            },
        ):
            env = opencode_environment(self.root)
        self.assertEqual(env["OPENCODE_CONFIG"], str(self.root / "opencode.json"))
        self.assertEqual(env["OPENAI_API_KEY"], "fixture-model-key")
        for key in (
            "OPENCODE_CONFIG_CONTENT",
            "OPENCODE_PERMISSION",
            "CODEX_HOME",
            "ZVEC_GREP_API_KEY",
            "HF_TOKEN",
        ):
            self.assertNotIn(key, env)

    def test_endpoint_cannot_embed_credentials_or_silently_use_plain_http(self):
        for endpoint in (
            "",
            "http://remote.example/v1",
            "https://user:key@example.test/v1",
            "https://example.test/v1?api_key=secret",
        ):
            with (
                self.subTest(endpoint=endpoint),
                patch.dict(os.environ, BROWSE_COMP_MODEL_BASE_URL=endpoint),
                self.assertRaises(ValueError),
            ):
                model_base_url()
        with patch.dict(
            os.environ, BROWSE_COMP_MODEL_BASE_URL="http://127.0.0.1:1234/v1/"
        ):
            self.assertEqual(model_base_url(), "http://127.0.0.1:1234/v1")


if __name__ == "__main__":
    unittest.main()
