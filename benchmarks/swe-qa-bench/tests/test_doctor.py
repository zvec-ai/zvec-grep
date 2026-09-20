from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from zg_bench import doctor


class RunDoctorTests(unittest.TestCase):
    def test_base_checks_reject_legacy_docker_compose(self) -> None:
        def fake_which(command: str) -> str | None:
            return f"/usr/bin/{command}"

        def fake_version(command: list[str]) -> tuple[bool, str]:
            if command[-2:] == ["compose", "version"]:
                return True, "docker-compose version 1.29.2"
            return True, "1.0.0"

        with (
            patch.object(doctor.shutil, "which", side_effect=fake_which),
            patch.object(doctor, "_run_version", side_effect=fake_version),
        ):
            checks = doctor.collect_checks()

        compose = next(check for check in checks if check.name == "Docker Compose")
        self.assertFalse(compose.ok)
        self.assertIn("v2 is required", compose.detail)

    def test_local_package_check_rejects_node_18_before_docker_run(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            source = Path(temp_dir)
            (source / "package.json").write_text("{}", encoding="utf-8")
            (source / "package-lock.json").write_text("{}", encoding="utf-8")
            tsc = source / "node_modules" / ".bin" / "tsc"
            tsc.parent.mkdir(parents=True)
            tsc.write_text("", encoding="utf-8")

            def fake_which(command: str) -> str | None:
                return f"/usr/bin/{command}" if command in {"node", "npm"} else None

            def fake_version(command: list[str]) -> tuple[bool, str]:
                if command[-1] == "--version" and "node" in command[0]:
                    return True, "v18.19.1"
                if command[-1] == "--version":
                    return True, "9.2.0"
                return True, "https://registry.npmjs.org/"

            with (
                patch.dict("os.environ", {"DASHSCOPE_API_KEY": "secret"}, clear=True),
                patch.object(doctor.shutil, "which", side_effect=fake_which),
                patch.object(doctor, "_run_version", side_effect=fake_version),
            ):
                checks = doctor._collect_run_checks(
                    agent="opencode",
                    model="aliyun-glm-5.2",
                    profiles=("zvec-grep",),
                    zvec_grep_package=str(source),
                )

            by_name = {check.name: check for check in checks}
            self.assertFalse(by_name["Node.js"].ok)
            self.assertIn("Node.js >=22", by_name["Node.js"].detail)
            self.assertTrue(by_name["Credentials"].ok)
            self.assertTrue(by_name["Local dependencies"].ok)
            self.assertTrue(by_name["Package lock registry"].ok)

    def test_internal_registry_in_lockfile_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            source = Path(temp_dir)
            (source / "package.json").write_text("{}", encoding="utf-8")
            (source / "package-lock.json").write_text(
                '"https://registry.anpm.alibaba-inc.com/pkg.tgz"',
                encoding="utf-8",
            )
            tsc = source / "node_modules" / ".bin" / "tsc"
            tsc.parent.mkdir(parents=True)
            tsc.write_text("", encoding="utf-8")

            with (
                patch.dict("os.environ", {"DASHSCOPE_API_KEY": "secret"}, clear=True),
                patch.object(doctor.shutil, "which", return_value="/usr/bin/tool"),
                patch.object(doctor, "_run_version", return_value=(True, "v22.0.0")),
            ):
                checks = doctor._collect_run_checks(
                    agent="opencode",
                    model="aliyun-glm-5.2",
                    profiles=("zvec-grep",),
                    zvec_grep_package=str(source),
                )

            lock_check = next(
                check for check in checks if check.name == "Package lock registry"
            )
            self.assertFalse(lock_check.ok)
            self.assertIn("registry.anpm.alibaba-inc.com", lock_check.detail)

    def test_internal_user_registry_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            source = Path(temp_dir)
            (source / "package.json").write_text("{}", encoding="utf-8")
            (source / "package-lock.json").write_text("{}", encoding="utf-8")
            tsc = source / "node_modules" / ".bin" / "tsc"
            tsc.parent.mkdir(parents=True)
            tsc.write_text("", encoding="utf-8")

            def fake_version(command: list[str]) -> tuple[bool, str]:
                if "config" in command:
                    return True, "https://registry.anpm.alibaba-inc.com"
                return True, "v22.0.0"

            with (
                patch.dict("os.environ", {"DASHSCOPE_API_KEY": "secret"}, clear=True),
                patch.object(doctor.shutil, "which", return_value="/usr/bin/tool"),
                patch.object(doctor, "_run_version", side_effect=fake_version),
            ):
                checks = doctor._collect_run_checks(
                    agent="opencode",
                    model="aliyun-glm-5.2",
                    profiles=("zvec-grep",),
                    zvec_grep_package=str(source),
                )

            registry_check = next(
                check for check in checks if check.name == "npm registry"
            )
            self.assertFalse(registry_check.ok)
            self.assertFalse(registry_check.required)
            self.assertIn("registry.npmjs.org", registry_check.detail)


if __name__ == "__main__":
    unittest.main()
