from __future__ import annotations

import json
import tempfile
import unittest
from collections import defaultdict
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch
from uuid import uuid4

from harbor.cli.job_plugins import attach_job_plugin
from harbor.models.job.config import RetryConfig
from harbor.models.trial.config import TaskConfig, TrialConfig
from harbor.trial.hooks import TrialEvent
from harbor.trial.queue import TrialQueue

from zg_bench.diagnostics import job_has_exceptions


class NativeTrialRetryTests(unittest.IsolatedAsyncioTestCase):
    async def _run_trials(
        self,
        job_dir: Path,
        outcomes: dict[str, list[str | None]],
        *,
        max_retries: int = 2,
    ) -> tuple[list[object], dict[str, int]]:
        """Use Harbor's real retry loop with local trials instead of Docker."""
        retry = RetryConfig(
            max_retries=max_retries,
            exclude_exceptions={"ApiUsageLimitError"},
            min_wait_sec=0,
        )
        queue = TrialQueue(n_concurrent=1, retry_config=retry)
        job = SimpleNamespace(
            config=SimpleNamespace(retry=retry),
            on_trial_ended=queue.on_trial_ended,
        )
        await attach_job_plugin(job, "zg_bench.retries:FailedTrialArchivePlugin")
        counts: dict[str, int] = defaultdict(int)

        class LocalTrial:
            def __init__(self, config: TrialConfig) -> None:
                self.config = config
                self.paths = SimpleNamespace(
                    trial_dir=config.trials_dir / config.trial_name
                )
                self.hooks = defaultdict(list)

            def add_hook(self, event: TrialEvent, hook: object) -> None:
                self.hooks[event].append(hook)

            async def run(self) -> object:
                name = self.config.trial_name
                counts[name] += 1
                exception_type = outcomes[name].pop(0)
                exception = (
                    SimpleNamespace(exception_type=exception_type)
                    if exception_type is not None
                    else None
                )
                result = SimpleNamespace(
                    id=uuid4(),
                    exception_info=exception,
                    verifier_result=SimpleNamespace(rewards={"reward": 0}),
                )
                self.paths.trial_dir.mkdir(parents=True)
                (self.paths.trial_dir / "result.json").write_text(
                    json.dumps(
                        {
                            "exception_info": (
                                {"exception_type": exception_type}
                                if exception_type is not None
                                else None
                            )
                        }
                    ),
                    encoding="utf-8",
                )
                (self.paths.trial_dir / "agent.log").write_text(
                    f"attempt {counts[name]}: {exception_type}", encoding="utf-8"
                )
                event = SimpleNamespace(
                    config=self.config, trial_name=name, result=result
                )
                for hook in self.hooks[TrialEvent.END]:
                    await hook(event)
                return result

        async def create_local_trial(config: TrialConfig) -> LocalTrial:
            return LocalTrial(config)

        configs = [
            TrialConfig(
                task=TaskConfig(path=Path("fixture-task")),
                trial_name=name,
                trials_dir=job_dir,
            )
            for name in outcomes
        ]
        with patch("harbor.trial.trial.Trial.create", side_effect=create_local_trial):
            results = [await queue.submit(config) for config in configs]
        return results, counts

    async def test_recovery_preserves_five_trials_and_archives_failed_attempts(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            job_dir = Path(temp_dir)
            outcomes = {f"trial-{i}": [None] for i in range(5)}
            outcomes["trial-0"] = ["AgentTimeoutError", None]
            outcomes["trial-4"] = ["RuntimeError", None]

            results, counts = await self._run_trials(job_dir, outcomes)

            self.assertEqual(len(results), 5)
            self.assertEqual(sum(counts.values()), 7)
            self.assertTrue(all(result.exception_info is None for result in results))
            self.assertEqual(len(list(job_dir.glob("*/result.json"))), 5)
            self.assertFalse(job_has_exceptions(job_dir))
            history_dir = job_dir / ".retry-history"
            history = json.loads((history_dir / "history.json").read_text())
            self.assertEqual(history["max_retries"], 2)
            for name in ("trial-0", "trial-4"):
                attempts = history["trials"][name]
                self.assertEqual(len(attempts), 2)
                self.assertIsNone(attempts[-1]["exception_type"])
                self.assertIsNone(attempts[-1]["archive"])
                archive = history_dir / attempts[0]["archive"]
                self.assertTrue((archive / "result.json").is_file())
                self.assertIn("attempt 1:", (archive / "agent.log").read_text())

    async def test_exhausted_timeout_stops_after_two_retries_and_stays_failed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            job_dir = Path(temp_dir)
            results, counts = await self._run_trials(
                job_dir,
                {"trial-one": ["AgentTimeoutError"] * 3 + [None]},
            )

            self.assertEqual(counts["trial-one"], 3)
            self.assertEqual(results[0].exception_info.exception_type, "AgentTimeoutError")
            self.assertTrue(job_has_exceptions(job_dir))
            history_dir = job_dir / ".retry-history"
            history = json.loads((history_dir / "history.json").read_text())
            self.assertEqual(len(history["trials"]["trial-one"]), 3)
            for attempt in range(1, 4):
                archive = history_dir / "trial-one" / f"attempt-{attempt}"
                self.assertTrue((archive / "result.json").is_file())

    async def test_successful_zero_reward_and_usage_limit_are_not_retried(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            results, counts = await self._run_trials(
                Path(temp_dir),
                {
                    "zero-reward": [None, None],
                    "usage-limited": ["ApiUsageLimitError", None],
                },
            )

            self.assertEqual(counts, {"zero-reward": 1, "usage-limited": 1})
            self.assertEqual(results[0].verifier_result.rewards["reward"], 0)
            self.assertEqual(results[1].exception_info.exception_type, "ApiUsageLimitError")


if __name__ == "__main__":
    unittest.main()
