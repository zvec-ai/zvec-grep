"""Keep the evidence Harbor removes when it retries a failed trial."""

from __future__ import annotations

import json
import shutil
from pathlib import Path
from typing import TYPE_CHECKING, Any

from harbor.models.job.plugin import BaseJobPlugin

if TYPE_CHECKING:
    from harbor.job import Job
    from harbor.models.job.result import JobResult
    from harbor.trial.hooks import TrialHookEvent


class FailedTrialArchivePlugin(BaseJobPlugin):
    """Archive failed END events before Harbor deletes their trial directories.

    Harbor 0.18 writes result.json before END hooks and keeps only the final
    attempt in each trial slot. History lives below a separate directory so
    collectors and diagnostics still see exactly the requested trial count.
    """

    async def on_job_start(self, job: Job) -> None:
        self._history: dict[str, Any] = {
            "schema_version": 1,
            "max_retries": job.config.retry.max_retries,
            "trials": {},
        }
        job.on_trial_ended(self._on_trial_ended)

    async def _on_trial_ended(self, event: TrialHookEvent) -> None:
        history_dir = event.config.trials_dir / ".retry-history"
        attempts = self._history["trials"].setdefault(event.trial_name, [])
        attempt = len(attempts) + 1
        exception = event.result.exception_info
        archive: Path | None = None
        if exception is not None:
            trial_dir = event.config.trials_dir / event.trial_name
            archive = Path(event.trial_name) / f"attempt-{attempt}"
            # Include terminal failures too, so the history is a complete
            # record rather than suggesting every archived failure recovered.
            shutil.copytree(trial_dir, history_dir / archive)

        attempts.append(
            {
                "attempt": attempt,
                "trial_id": str(event.result.id),
                "exception_type": (
                    exception.exception_type if exception is not None else None
                ),
                "archive": str(archive) if archive is not None else None,
            }
        )
        history_dir.mkdir(parents=True, exist_ok=True)
        (history_dir / "history.json").write_text(
            json.dumps(self._history, indent=2) + "\n", encoding="utf-8"
        )

    async def on_job_end(self, job_result: JobResult) -> None:
        pass
