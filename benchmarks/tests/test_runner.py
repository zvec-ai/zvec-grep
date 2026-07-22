from __future__ import annotations

import hashlib
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from zg_bench import runner


class LocalPackageTests(unittest.TestCase):
    def test_packs_current_checkout_and_names_artifact_by_digest(self) -> None:
        package_contents = b"local zvec-grep package"

        with tempfile.TemporaryDirectory() as temp_dir:
            cache_dir = Path(temp_dir) / "cache"
            source_dir = Path(temp_dir) / "source"
            (source_dir / "node_modules" / ".bin").mkdir(parents=True)
            (source_dir / "package.json").write_text("{}")
            (source_dir / "node_modules" / ".bin" / "tsc").write_text("")

            def fake_run(
                command: list[str], **kwargs: object
            ) -> subprocess.CompletedProcess[str]:
                self.assertEqual(command[:2], ["npm", "pack"])
                self.assertIn("npm_config_cache", kwargs["env"])
                pack_dir = Path(command[-1])
                (pack_dir / "zvec-zvec-grep-0.1.5.tgz").write_bytes(package_contents)
                return subprocess.CompletedProcess(command, 0, stdout="", stderr="")

            with (
                patch.object(runner, "LOCAL_PACKAGE_DIR", cache_dir),
                patch.object(runner, "LOCAL_NPM_CACHE_DIR", cache_dir / "npm-cache"),
                patch.object(runner.shutil, "which", return_value="/usr/bin/npm"),
                patch.object(runner.subprocess, "run", side_effect=fake_run),
            ):
                package, digest = runner.prepare_local_zvec_grep_package(source_dir)

            expected_digest = hashlib.sha256(package_contents).hexdigest()
            self.assertEqual(digest, expected_digest)
            self.assertEqual(
                package,
                (cache_dir / f"zvec-grep-{expected_digest[:16]}.tgz").resolve(),
            )
            self.assertEqual(package.read_bytes(), package_contents)

    def test_zvec_profile_mounts_local_package_and_keys_volume_by_hash(self) -> None:
        digest = "b" * 64

        with tempfile.TemporaryDirectory() as temp_dir:
            cache_dir = Path(temp_dir) / "agent-setup"
            source_dir = Path(temp_dir) / "source"
            source_dir.mkdir()
            package = Path(temp_dir) / "zvec-grep.tgz"
            package.write_bytes(b"package")
            inspected = subprocess.CompletedProcess([], 0, stdout="", stderr="")

            with (
                patch.object(runner, "SETUP_CACHE_DIR", cache_dir),
                patch.object(
                    runner,
                    "prepare_local_zvec_grep_package",
                    return_value=(package, digest),
                ),
                patch.object(runner.subprocess, "run", return_value=inspected),
            ):
                prepared = runner.prepare_setup_cache(
                    "opencode",
                    "zvec-grep",
                    zvec_grep_package=str(source_dir),
                )

            overlay = json.loads(prepared.compose_path.read_text())
            service_volumes = overlay["services"]["main"]["volumes"]
            self.assertEqual(
                service_volumes[1],
                {
                    "type": "bind",
                    "source": str(package),
                    "target": runner.LOCAL_ZVEC_GREP_PACKAGE_TARGET,
                    "read_only": True,
                },
            )
            self.assertIn(
                f"local-{digest[:16]}",
                overlay["volumes"]["agent-setup-cache"]["name"],
            )
            self.assertEqual(
                prepared.zvec_grep_package,
                runner.LOCAL_ZVEC_GREP_PACKAGE_TARGET,
            )
            self.assertEqual(prepared.zvec_grep_package_sha256, digest)

    def test_version_shorthand_selects_published_npm_package(self) -> None:
        prepared = runner.prepare_zvec_grep_package("0.1.5")

        self.assertEqual(prepared.install_spec, "@zvec/zvec-grep@0.1.5")
        self.assertIsNone(prepared.bind_source)
        self.assertIsNone(prepared.sha256)

    def test_harbor_command_installs_mounted_package_and_records_hash(self) -> None:
        digest = "c" * 64
        suite = runner.SmokeSuite(
            name="swebench-verified",
            dataset="swe-bench/swe-bench-verified@2",
            task="swe-bench/pallets__flask-5014",
        )

        command = runner.build_harbor_command(
            suite,
            profile="zvec-grep",
            agent="opencode",
            model="aliyun-glm-5.2",
            job_name="local-package-test",
            zvec_grep_package=runner.LOCAL_ZVEC_GREP_PACKAGE_TARGET,
            zvec_grep_package_sha256=digest,
        )

        self.assertIn(
            f"zvec_grep_package={runner.LOCAL_ZVEC_GREP_PACKAGE_TARGET}", command
        )
        self.assertIn(f"zvec_grep_package_sha256={digest}", command)


if __name__ == "__main__":
    unittest.main()
