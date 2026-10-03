from __future__ import annotations

import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

from zg_bench import cli, index
from zg_bench.artifacts import read_json, write_json
from zg_bench.config import load_config
from zg_bench.corpus import workspace_root
from zg_bench.process import CommandResult


def result(code=0, stdout="ready"):
    return CommandResult((), code, stdout, "")


class IndexResumeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.artifacts = Path(self.temp.name)
        self.config = load_config()
        self.config = replace(self.config, zvec_grep=replace(
            self.config.zvec_grep, embedding="local/test"))
        self.root = workspace_root(self.artifacts, "zvec-grep")
        self.root.mkdir(parents=True)
        write_json(self.artifacts / "state/corpus.json", {"fingerprint": "corpus-v1"})
        for target, value in (("resolve_executable", Path("/fake/zg")),
                              ("run_command", result())):
            mock = patch.object(index, target, return_value=value)
            mock.start()
            self.addCleanup(mock.stop)
        self.commands = []

    def run_attempt(self, outcome):
        def run(command, **kwargs):
            self.commands.append(command)
            (self.root / ".zvec-grep").mkdir(exist_ok=True)
            kwargs["stdout_log"].parent.mkdir(parents=True, exist_ok=True)
            kwargs["stdout_log"].write_text("attempt output")
            if isinstance(outcome, BaseException):
                raise outcome
            return outcome
        return patch.object(index, "run_streaming_command", side_effect=run)

    def interrupt(self):
        with self.run_attempt(KeyboardInterrupt()), self.assertRaises(KeyboardInterrupt):
            index.build_index(self.config, self.artifacts)

    def test_interrupt_then_resume_preserves_logs_and_time(self):
        with patch.object(index.time, "monotonic", side_effect=[10, 13]):
            self.interrupt()
        self.assertTrue(index.resumable_index(self.config, self.artifacts))
        self.assertIsNone(index.prepared_index(self.config, self.artifacts))
        with self.run_attempt(result()), patch.object(index.time, "monotonic", side_effect=[20, 25]):
            state = read_json(index.build_index(self.config, self.artifacts))
        self.assertEqual(state["build_wall_seconds"], 8)
        self.assertTrue(state["build_time_complete"])
        self.assertEqual(len(state["attempts"]), 2)
        self.assertEqual(len({a["stdout_log"] for a in state["attempts"]}), 2)
        for attempt in state["attempts"]:
            self.assertTrue(Path(attempt["stdout_log"]).is_file())
        self.assertNotIn("--rebuild", self.commands[-1])
        self.assertIn("--embedding-concurrency", self.commands[-1])
        self.assertEqual(self.commands[-1][1], "index")
        self.assertNotIn("--index-embedding-concurrency", self.commands[-1])
        self.assertFalse((self.artifacts / "state/index.pending.json").exists())
        with patch.object(index, "run_streaming_command") as run:
            index.build_index(self.config, self.artifacts)
            run.assert_not_called()

    def test_failed_attempt_can_resume(self):
        with self.run_attempt(result(1)), self.assertRaises(RuntimeError):
            index.build_index(self.config, self.artifacts)
        self.assertTrue(index.resumable_index(self.config, self.artifacts))

    def test_failed_ready_check_keeps_resume_record(self):
        with self.run_attempt(result()), patch.object(
            index, "run_command", side_effect=[result(), result(1, "not ready")]
        ), self.assertRaises(RuntimeError):
            index.build_index(self.config, self.artifacts)
        self.assertTrue(index.resumable_index(self.config, self.artifacts))
        self.assertIsNone(index.prepared_index(self.config, self.artifacts))
        with self.run_attempt(result()):
            index.build_index(self.config, self.artifacts)
        self.assertNotIn("--rebuild", self.commands[-1])

    def test_input_changes_reject_resume(self):
        self.interrupt()
        for field, value in (("embedding", "local/other"), ("max_filesize", "2M"), ("device", "other")):
            config = replace(self.config, zvec_grep=replace(self.config.zvec_grep, **{field: value}))
            self.assertFalse(index.resumable_index(config, self.artifacts))
        write_json(self.artifacts / "state/corpus.json", {"fingerprint": "corpus-v2"})
        self.assertFalse(index.resumable_index(self.config, self.artifacts))

    def test_unknown_partial_index_is_not_resumable(self):
        (self.root / ".zvec-grep").mkdir()
        self.assertFalse(index.resumable_index(self.config, self.artifacts))

    def test_interrupted_rebuild_is_not_assumed_safe(self):
        with self.run_attempt(KeyboardInterrupt()), self.assertRaises(KeyboardInterrupt):
            index.build_index(self.config, self.artifacts, rebuild=True)
        self.assertFalse(index.resumable_index(self.config, self.artifacts))
        self.assertIn("--rebuild", self.commands[-1])

    def test_hard_kill_missing_duration_is_explicit(self):
        self.interrupt()
        path = self.artifacts / "state/index.pending.json"
        state = read_json(path)
        del state["attempts"][0]["wall_seconds"]
        write_json(path, state)
        with self.run_attempt(result()):
            state = read_json(index.build_index(self.config, self.artifacts))
        self.assertFalse(state["build_time_complete"])

    def test_prepare_resumes_but_changed_config_rebuilds(self):
        for changed in (False, True):
            with self.subTest(changed=changed):
                self.interrupt()
                config = self.config if not changed else replace(
                    self.config, zvec_grep=replace(self.config.zvec_grep, embedding="local/other"))
                with patch.object(cli, "load_config", return_value=config), patch.object(
                    cli, "DEFAULT_ARTIFACTS_DIR", self.artifacts
                ), patch.object(cli, "prepared_dataset", return_value=Path("dataset")), patch.object(
                    cli, "prepared_corpus", return_value=Path("corpus")
                ), self.run_attempt(result()):
                    self.assertEqual(cli.main(["prepare", "--yes"]), 0)
                self.assertEqual("--rebuild" in self.commands[-1], changed)
                (self.artifacts / "state/index.json").unlink()


if __name__ == "__main__":
    unittest.main()
