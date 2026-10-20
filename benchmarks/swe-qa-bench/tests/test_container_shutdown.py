"""Real Harbor/Docker signal regression, enabled explicitly by the offline CI.

No Agent or model is invoked. The control preserves Harbor's original keepalive
and the fixed cases use the production overlay and the unchanged stop timeout.
"""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
import tempfile
import time
import unittest
import uuid
from pathlib import Path
from unittest.mock import patch

from harbor.environments.docker.docker import DockerEnvironment
from harbor.models.task.config import EnvironmentConfig
from harbor.models.trial.paths import TrialPaths
from zg_bench import runner


@unittest.skipUnless(
    os.environ.get("ZG_BENCH_RUN_DOCKER_TESTS") == "1",
    "set ZG_BENCH_RUN_DOCKER_TESTS=1 to run the real Docker shutdown regression",
)
class ContainerShutdownTests(unittest.IsolatedAsyncioTestCase):
    @classmethod
    def setUpClass(cls) -> None:
        # Fail rather than silently skipping when CI explicitly requests Docker.
        subprocess.run(["docker", "info"], check=True, capture_output=True, timeout=30)
        subprocess.run(
            ["docker", "pull", "ubuntu:24.04"],
            check=True,
            capture_output=True,
            timeout=180,
        )

    async def shutdown_case(self, root: Path, profile: str, *, legacy: bool) -> dict:
        name = f"zg-shutdown-{uuid.uuid4().hex}"
        with (
            patch.object(runner, "SETUP_CACHE_DIR", root / "cache"),
            patch.object(runner, "setup_cache_volume_name", return_value=name),
        ):
            prepared = runner.prepare_setup_cache("opencode", profile)
        overlay = json.loads(prepared.compose_path.read_text())
        if legacy:
            del overlay["services"]["main"]["init"]
            prepared.compose_path.write_text(json.dumps(overlay))

        environment_dir = root / "environment"
        environment_dir.mkdir()
        (environment_dir / "Dockerfile").write_text("FROM ubuntu:24.04\n")
        paths = TrialPaths(root / "trial")
        paths.mkdir()
        environment = DockerEnvironment(
            environment_dir=environment_dir,
            environment_name=name,
            session_id=name,
            trial_paths=paths,
            task_env_config=EnvironmentConfig(docker_image="ubuntu:24.04"),
            extra_docker_compose=[prepared.compose_path],
        )
        stopped = False
        try:
            await environment.start(force_build=False)
            ps = await environment._run_docker_compose_command(["ps", "-q", "main"])
            container_id = ps.stdout.strip()
            inspect = json.loads(
                await asyncio.to_thread(
                    subprocess.check_output,
                    ["docker", "inspect", container_id],
                    text=True,
                    timeout=10,
                )
            )[0]
            self.assertEqual(inspect["Config"]["Cmd"], ["sh", "-c", "sleep infinity"])
            self.assertEqual(bool(inspect["HostConfig"].get("Init")), not legacy)
            pid1 = await environment.exec(command="cat /proc/1/comm")

            # Exercise Harbor's actual transfer path before stopping its container.
            for filename, content in {
                "answer.txt": "completed answer\n",
                "session-usage.json": '{"input_tokens":123,"complete":true}\n',
                "reward.txt": "1\n",
            }.items():
                source = root / filename
                source.write_text(content)
                await environment.upload_file(source, f"/tmp/{filename}")
                destination = root / "collected" / filename
                destination.parent.mkdir(exist_ok=True)
                await environment.download_file(f"/tmp/{filename}", destination)
                self.assertEqual(destination.read_bytes(), source.read_bytes())
            evidence = {str(p): p.read_bytes() for p in (root / "collected").iterdir()}

            since = str(time.time())
            started = time.monotonic()
            await environment.stop(delete=True)
            stopped = True
            elapsed = time.monotonic() - started
            # Replay daemon events after down: the removed container cannot be inspected.
            events = await asyncio.to_thread(
                subprocess.check_output,
                [
                    "docker",
                    "events",
                    "--since",
                    since,
                    "--until",
                    str(time.time()),
                    "--filter",
                    f"container={container_id}",
                    "--format",
                    "{{json .}}",
                ],
                text=True,
                timeout=15,
            )
            decoded = [json.loads(line) for line in events.splitlines() if line]
            exit_codes = [
                int(event["Actor"]["Attributes"]["exitCode"])
                for event in decoded
                if event.get("Action") == "die"
            ]
            signals = [
                event["Actor"]["Attributes"].get("signal")
                for event in decoded
                if event.get("Action") == "kill"
            ]
            remaining = await asyncio.to_thread(
                subprocess.check_output,
                ["docker", "ps", "-aq", "--filter", f"id={container_id}"],
                text=True,
                timeout=10,
            )
            self.assertEqual(remaining.strip(), "")
            for filename, content in evidence.items():
                self.assertEqual(Path(filename).read_bytes(), content)
            result = {
                "profile": profile,
                "legacy": legacy,
                "pid1": pid1.stdout.strip(),
                "shutdown_seconds": elapsed,
                "exit_codes": exit_codes,
                "signals": signals,
                "evidence_files": len(evidence),
            }
            print("CONTAINER_SHUTDOWN " + json.dumps(result), flush=True)
            return result
        finally:
            # Use only this test's UUID-scoped resources, including on assertion failure.
            if not stopped:
                await environment.stop(delete=True)
            await asyncio.to_thread(
                subprocess.run,
                ["docker", "volume", "rm", name],
                check=True,
                capture_output=True,
                text=True,
                timeout=30,
            )

    async def test_init_avoids_forced_kill_and_preserves_collected_evidence(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            control = await self.shutdown_case(root / "legacy", "baseline", legacy=True)
            self.assertEqual(control["exit_codes"], [137])
            self.assertIn("9", control["signals"])
            self.assertGreaterEqual(control["shutdown_seconds"], 8)
            for profile in runner.PROFILES:
                with self.subTest(profile=profile):
                    fixed = await self.shutdown_case(
                        root / profile, profile, legacy=False
                    )
                    self.assertEqual(len(fixed["exit_codes"]), 1)
                    self.assertNotEqual(fixed["exit_codes"], [137])
                    self.assertNotIn("9", fixed["signals"])
                    self.assertLess(
                        fixed["shutdown_seconds"] + 5, control["shutdown_seconds"]
                    )
