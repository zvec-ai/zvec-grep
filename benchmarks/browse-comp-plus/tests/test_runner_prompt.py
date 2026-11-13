from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from zg_bench.config import load_config
from zg_bench.models import AttemptResult, TraceSummary
from zg_bench.runner import _prompt, _run_profile


class RunnerPromptTests(unittest.TestCase):
    def test_each_profile_receives_its_exact_absolute_workspace(self):
        config = load_config()
        with tempfile.TemporaryDirectory() as directory:
            artifacts = (
                Path(directory)
                / "repo with spaces"
                / "benchmarks"
                / "browse-comp-plus"
                / "artifacts"
            )
            run_root = artifacts / "runs" / "fixture"
            query = "Which building is described?"
            prompts = []
            for profile, name, other_name in (
                ("baseline", "a", "b"),
                ("zvec-grep", "b", "a"),
            ):
                with (
                    self.subTest(profile=profile),
                    patch("zg_bench.runner.run_attempt") as run_attempt,
                    patch("zg_bench.runner.validate_workspace"),
                ):
                    run_attempt.return_value = AttemptResult(
                        query_id="966",
                        profile=profile,
                        trial_index=1,
                        status="completed",
                        attempt=1,
                        started_at="start",
                        finished_at="finish",
                        wall_seconds=1,
                        exit_code=0,
                        infrastructure_failure=False,
                        trace=TraceSummary(
                            thread_id="fixture",
                            final_response="Answer",
                            last_agent_message="Answer",
                            turn_completed=True,
                            usage=None,
                            tool_calls=(),
                            observed_docids=(),
                        ),
                    )
                    _run_profile(
                        config,
                        artifacts,
                        run_root,
                        {"query_id": "966", "query": query},
                        profile,
                        1,
                        config.run.model,
                        config.run.reasoning_effort,
                        run_root / "profiles",
                        "opencode",
                    )
                    run_attempt.assert_called_once()
                    prompt = run_attempt.call_args.kwargs["prompt"]
                    workspace = (artifacts / "workspaces" / name).resolve()
                    other = (artifacts / "workspaces" / other_name).resolve()
                    self.assertIn(str(workspace), prompt)
                    self.assertNotIn(str(other), prompt)
                    self.assertNotIn("{workspace_root}", prompt)
                    self.assertNotIn("{query}", prompt)
                    self.assertIn(query, prompt)
                    prompts.append(prompt.replace(str(workspace), "<workspace>"))
            self.assertEqual(prompts[0], prompts[1])

    def test_query_placeholders_and_whitespace_are_preserved_verbatim(self):
        query = "  Literal {workspace_root} and {query}.\nKeep punctuation: - / {}.  "
        workspace = Path(
            "benchmarks/browse-comp-plus/artifacts with spaces/workspaces/b"
        )
        prompt = _prompt(query, workspace)
        preamble, question = prompt.split("Question:\n\n", 1)
        rendered_query, _ = question.split("\n\nReturn your response", 1)
        self.assertEqual(rendered_query, query)
        self.assertIn(str(workspace.resolve()), preamble)
        self.assertNotIn("{workspace_root}", preamble)


if __name__ == "__main__":
    unittest.main()
