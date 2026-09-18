from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import yaml


ROOT = Path(__file__).resolve().parents[3]
WORKFLOW_PATH = ROOT / ".github/workflows/swe-qa-bench.yml"


class ManualBenchmarkAuthorizationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = yaml.load(WORKFLOW_PATH.read_text(), Loader=yaml.BaseLoader)
        cls.guard = cls.workflow["jobs"]["validate"]["steps"][0]

    def run_guard(
        self,
        roles: dict[str, str],
        *,
        actor: str = "original",
        triggering_actor: str = "rerunner",
        event: str = "workflow_dispatch",
        api_errors: tuple[str, ...] = (),
    ) -> tuple[subprocess.CompletedProcess[str], list[str]]:
        """Run the workflow's real Bash guard against a local permission API stub."""
        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory)
            log_path = fixture / "permission-lookups.jsonl"
            fake_gh = fixture / "gh"
            fake_gh.write_text(
                f"#!{sys.executable}\n"
                "import json, os, sys\n"
                "args = sys.argv[1:]\n"
                "if len(args) != 4 or args[0] != 'api' or "
                "args[2:] != ['--jq', '.role_name']:\n"
                "    sys.exit('unexpected permission API invocation')\n"
                "prefix = 'repos/fixture/benchmark/collaborators/'\n"
                "if not args[1].startswith(prefix) or not args[1].endswith('/permission'):\n"
                "    sys.exit('unexpected repository or permission endpoint')\n"
                "actor = args[1][len(prefix):-len('/permission')]\n"
                "with open(os.environ['FAKE_GH_LOG'], 'a') as log:\n"
                "    log.write(json.dumps(actor) + '\\n')\n"
                "if actor in json.loads(os.environ['FAKE_GH_ERRORS']):\n"
                "    print('HTTP 403: permission lookup denied', file=sys.stderr)\n"
                "    sys.exit(1)\n"
                "print(json.loads(os.environ['FAKE_GH_ROLES']).get(actor, 'none'))\n"
            )
            fake_gh.chmod(0o755)
            result = subprocess.run(
                ["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", self.guard["run"]],
                cwd=fixture,
                env={
                    "PATH": str(fixture) + os.pathsep + "/usr/bin:/bin",
                    "BENCH_EVENT_NAME": event,
                    "BENCH_REPOSITORY": "fixture/benchmark",
                    "BENCH_ACTOR": actor,
                    "BENCH_TRIGGERING_ACTOR": triggering_actor,
                    "GH_TOKEN": "local-test-token",
                    "FAKE_GH_LOG": str(log_path),
                    "FAKE_GH_ERRORS": json.dumps(api_errors),
                    "FAKE_GH_ROLES": json.dumps(roles),
                },
                capture_output=True,
                text=True,
                timeout=10,
            )
            lookups = (
                [json.loads(line) for line in log_path.read_text().splitlines()]
                if log_path.exists()
                else []
            )
            return result, lookups

    def test_only_manual_trigger_and_existing_model_choices(self) -> None:
        self.assertEqual(set(self.workflow["on"]), {"workflow_dispatch"})
        model = self.workflow["on"]["workflow_dispatch"]["inputs"]["model"]
        self.assertEqual(model["default"], "glm-5.2")
        self.assertEqual(set(model["options"]), {"glm-5.2", "qwen3.8-max"})

    def test_every_job_checks_current_permissions_before_checkout_or_model_secrets(self) -> None:
        self.assertEqual(self.guard["name"], "Authorize manual benchmark run")
        self.assertNotIn("if", self.guard)
        self.assertNotEqual(self.guard.get("continue-on-error"), "true")
        self.assertEqual(
            self.guard["env"],
            {
                "GH_TOKEN": "${{ github.token }}",
                "BENCH_EVENT_NAME": "${{ github.event_name }}",
                "BENCH_REPOSITORY": "${{ github.repository }}",
                "BENCH_ACTOR": "${{ github.actor }}",
                "BENCH_TRIGGERING_ACTOR": "${{ github.triggering_actor }}",
            },
        )
        for job_name, job in self.workflow["jobs"].items():
            with self.subTest(job=job_name):
                self.assertEqual(job["steps"][0], self.guard)
                self.assertNotIn("secrets.", json.dumps(job.get("env", {})))
                checkouts = [
                    index
                    for index, step in enumerate(job["steps"])
                    if step.get("uses", "").startswith("actions/checkout@")
                ]
                self.assertTrue(checkouts)
                self.assertGreater(min(checkouts), 0)
                for step in job["steps"][1:]:
                    # Cleanup and uploads must not run or expose model
                    # credentials if this job's authorization step failed.
                    condition = step.get("if", "")
                    if "always()" in condition:
                        guard_id = self.guard["id"]
                        self.assertIn(f"steps.{guard_id}.outcome == 'success'", condition)

    def test_admin_and_maintain_are_allowed_for_both_original_actor_and_rerunner(self) -> None:
        for original_role, rerunner_role in (("admin", "maintain"), ("maintain", "admin")):
            with self.subTest(original_role=original_role, rerunner_role=rerunner_role):
                result, lookups = self.run_guard(
                    {"original": original_role, "rerunner": rerunner_role}
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(lookups, ["original", "rerunner"])

    def test_non_core_and_custom_roles_are_denied(self) -> None:
        for role in ("write", "triage", "read", "none", "custom-maintainer"):
            with self.subTest(role=role):
                result, lookups = self.run_guard({"original": role, "rerunner": "admin"})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("original", lookups)

    def test_rerun_checks_both_rerunner_and_original_actor_current_role(self) -> None:
        for roles in (
            {"original": "admin", "rerunner": "write"},
            {"original": "read", "rerunner": "admin"},
        ):
            with self.subTest(roles=roles):
                result, _ = self.run_guard(roles)
                self.assertNotEqual(result.returncode, 0)

    def test_non_manual_events_and_missing_actors_fail_before_permission_lookup(self) -> None:
        cases = (
            {"event": "pull_request"},
            {"event": "pull_request_target"},
            {"event": "push"},
            {"event": "schedule"},
            {"actor": ""},
            {"triggering_actor": ""},
        )
        for arguments in cases:
            with self.subTest(arguments=arguments):
                result, lookups = self.run_guard(
                    {"original": "admin", "rerunner": "admin"}, **arguments
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(lookups, [])

    def test_permission_api_failure_denies_execution(self) -> None:
        for actor in ("original", "rerunner"):
            with self.subTest(actor=actor):
                result, lookups = self.run_guard(
                    {"original": "admin", "rerunner": "maintain"},
                    api_errors=(actor,),
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(actor, lookups)


if __name__ == "__main__":
    unittest.main()
