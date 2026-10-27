from __future__ import annotations

import io
import json
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest.mock import Mock, patch
from urllib.error import HTTPError, URLError

from zg_bench.ci_prepared_artifact import (
    ARTIFACT_PREFIX,
    ArtifactDiscoveryError,
    GitHubAPI,
    discover,
    main,
)

REPOSITORY = "zvec-ai/zvec-grep"
BRANCH = "dev/browse-comp-plus-ci"
KEY = "Linux-X64-browse-comp-runtime-v3-" + "a" * 64
NAME = ARTIFACT_PREFIX + KEY
LEGACY_KEY = "Linux-X64-browse-comp-prepared-v2-" + "b" * 64
LEGACY_NAME = ARTIFACT_PREFIX + LEGACY_KEY
ENDPOINT = f"/repos/{REPOSITORY}/actions"


def artifact(run_id=100, *, name=NAME, expired=False, branch=BRANCH):
    return {
        "id": run_id + 1000,
        "name": name,
        "expired": expired,
        "workflow_run": {"id": run_id, "head_branch": branch},
    }


def run(run_id=100, **updates):
    return {
        "id": run_id,
        "repository": {"full_name": REPOSITORY},
        "head_repository": {"full_name": REPOSITORY},
        "head_branch": BRANCH,
        "event": "workflow_dispatch",
        "path": ".github/workflows/zg-bench.yml",
        **updates,
    }


def plan_artifact(run_id=100, **updates):
    return artifact(run_id, name=f"browse-comp-plus-plan-{run_id}", **updates)


def prepared_artifacts(run_id=100):
    return {"artifacts": [artifact(run_id), plan_artifact(run_id)]}


class PreparedArtifactTests(unittest.TestCase):
    def discover(self, api, **updates):
        return discover(
            api,
            **{
                "key": KEY,
                "repository": REPOSITORY,
                "branch": BRANCH,
                "current_run_id": "200",
                **updates,
            },
        )

    def test_exact_artifact_uses_filtered_list_and_one_run_lookup(self):
        api = Mock(
            side_effect=[{"artifacts": [artifact()]}, run(), prepared_artifacts()]
        )
        result = self.discover(api)
        self.assertEqual(result.run_id, 100)
        self.assertEqual(result.artifact_name, NAME)
        self.assertFalse(result.legacy)
        self.assertEqual(api.call_count, 3)
        self.assertEqual(
            api.call_args_list[0].args[0],
            f"{ENDPOINT}/artifacts?per_page=100&page=1&name={NAME}",
        )
        self.assertEqual(api.call_args_list[1].args[0], f"{ENDPOINT}/runs/100")
        self.assertEqual(
            api.call_args_list[2].args[0],
            f"{ENDPOINT}/runs/100/artifacts?per_page=100&page=1",
        )

    def test_empty_list_is_a_miss(self):
        api = Mock(return_value={"artifacts": []})
        self.assertIsNone(self.discover(api).run_id)
        api.assert_called_once()

    def test_runtime_miss_converts_only_the_exact_compatible_full_snapshot(self):
        api = Mock(
            side_effect=[
                {"artifacts": []},
                {"artifacts": [artifact(name=LEGACY_NAME)]},
                run(),
                {"artifacts": [artifact(name=LEGACY_NAME), plan_artifact()]},
            ]
        )
        result = self.discover(api, legacy_key=LEGACY_KEY)
        self.assertEqual(result.run_id, 100)
        self.assertEqual(result.artifact_name, LEGACY_NAME)
        self.assertTrue(result.legacy)
        self.assertEqual(api.call_count, 4)
        self.assertIn(f"name={NAME}", api.call_args_list[0].args[0])
        self.assertIn(f"name={LEGACY_NAME}", api.call_args_list[1].args[0])

    def test_exact_runtime_match_does_not_download_or_look_up_full_snapshots(self):
        api = Mock(
            side_effect=[{"artifacts": [artifact()]}, run(), prepared_artifacts()]
        )
        result = self.discover(api, legacy_key=LEGACY_KEY)
        self.assertFalse(result.legacy)
        self.assertEqual(api.call_count, 3)

    def test_explicit_compatible_full_snapshot_still_requires_same_run_plan(self):
        for with_plan in (False, True):
            with self.subTest(with_plan=with_plan):
                records = [artifact(name=LEGACY_NAME)]
                if with_plan:
                    records.append(plan_artifact())
                api = Mock(side_effect=[run(), {"artifacts": records}])
                if with_plan:
                    result = self.discover(api, legacy_key=LEGACY_KEY, source_run_id="100")
                    self.assertTrue(result.legacy)
                    self.assertEqual(result.artifact_name, LEGACY_NAME)
                else:
                    with self.assertRaisesRegex(ArtifactDiscoveryError, "no unexpired plan"):
                        self.discover(api, legacy_key=LEGACY_KEY, source_run_id="100")

    def test_invalid_compatible_key_fails_before_api_calls(self):
        for legacy_key in (KEY, "../snapshot", "key\n"):
            with self.subTest(legacy_key=legacy_key):
                api = Mock()
                with self.assertRaisesRegex(ArtifactDiscoveryError, "invalid legacy"):
                    self.discover(api, legacy_key=legacy_key)
                api.assert_not_called()

    def test_filters_expired_wrong_name_current_run_and_branch_before_lookup(self):
        api = Mock(
            side_effect=[
                {
                    "artifacts": [
                        artifact(expired=True),
                        artifact(name=NAME + "-different"),
                        artifact(200),
                        artifact(branch="main"),
                        artifact(),
                    ]
                },
                run(),
                prepared_artifacts(),
            ]
        )
        self.assertEqual(self.discover(api).run_id, 100)
        self.assertEqual(api.call_count, 3)

    def test_rejects_untrusted_run_repository_branch_event_and_workflow(self):
        variants = (
            {"repository": {"full_name": "fork/zvec-grep"}},
            {"head_repository": {"full_name": "fork/zvec-grep"}},
            {"head_branch": "main"},
            {"event": "pull_request"},
            {"path": ".github/workflows/other.yml"},
            {"id": 999},
        )
        for updates in variants:
            with self.subTest(updates=updates):
                api = Mock(side_effect=[{"artifacts": [artifact()]}, run(**updates)])
                self.assertIsNone(self.discover(api).run_id)

    def test_both_manual_workflow_entry_points_are_allowed(self):
        for workflow in ("zg-bench.yml", "browse-comp-plus.yml"):
            with self.subTest(workflow=workflow):
                api = Mock(
                    side_effect=[
                        {"artifacts": [artifact()]},
                        run(path=f".github/workflows/{workflow}"),
                        prepared_artifacts(),
                    ]
                )
                self.assertEqual(self.discover(api).run_id, 100)

    def test_checks_each_run_once_when_list_has_multiple_artifacts(self):
        api = Mock(
            side_effect=[
                {"artifacts": [artifact(), artifact(), artifact(101)]},
                run(event="push"),
                run(101),
                prepared_artifacts(101),
            ]
        )
        self.assertEqual(self.discover(api).run_id, 101)
        self.assertEqual(api.call_count, 4)

    def test_pagination_finds_a_later_matching_artifact(self):
        api = Mock(
            side_effect=[
                {"artifacts": [artifact(expired=True)]},
                {"artifacts": [artifact()]},
                run(),
                {"artifacts": [artifact()]},
                {"artifacts": [plan_artifact()]},
                {"artifacts": []},
            ]
        )
        with patch("zg_bench.ci_prepared_artifact.PAGE_SIZE", 1):
            self.assertEqual(self.discover(api).run_id, 100)
        self.assertIn("page=2", api.call_args_list[1].args[0])

    def test_pagination_limit_fails_instead_of_rebuilding(self):
        api = Mock(return_value={"artifacts": [artifact(expired=True)]})
        with (
            patch("zg_bench.ci_prepared_artifact.PAGE_SIZE", 1),
            patch("zg_bench.ci_prepared_artifact.MAX_PAGES", 2),
            self.assertRaisesRegex(ArtifactDiscoveryError, "pagination limit"),
        ):
            self.discover(api)
        self.assertEqual(api.call_count, 2)

    def test_explicit_run_uses_legacy_name_and_marks_for_republication(self):
        legacy_name = ARTIFACT_PREFIX + "100"
        api = Mock(side_effect=[run(), {"artifacts": [artifact(name=legacy_name)]}])
        result = self.discover(api, source_run_id="100")
        self.assertEqual(result.run_id, 100)
        self.assertEqual(result.artifact_name, legacy_name)
        self.assertTrue(result.legacy)
        self.assertEqual(api.call_count, 2)
        self.assertIn("/runs/100/artifacts?", api.call_args_list[1].args[0])

    def test_explicit_run_prefers_exact_key_over_legacy(self):
        api = Mock(
            side_effect=[
                run(),
                {
                    "artifacts": [
                        artifact(name=ARTIFACT_PREFIX + "100"),
                        artifact(),
                        plan_artifact(),
                    ]
                },
            ]
        )
        result = self.discover(api, source_run_id="100")
        self.assertEqual(result.artifact_name, NAME)
        self.assertFalse(result.legacy)
        self.assertEqual(api.call_count, 2)

    def test_automatic_skips_missing_expired_or_wrong_run_plan_and_finds_next(self):
        for invalid_plans in (
            [],
            [plan_artifact(expired=True)],
            [artifact(999, name="browse-comp-plus-plan-100")],
            [artifact(name="browse-comp-plus-plan-100-other")],
        ):
            with self.subTest(plans=invalid_plans):
                api = Mock(
                    side_effect=[
                        {"artifacts": [artifact(), artifact(101)]},
                        run(),
                        {"artifacts": [artifact(), *invalid_plans]},
                        run(101),
                        prepared_artifacts(101),
                    ]
                )
                self.assertEqual(self.discover(api).run_id, 101)

    def test_automatic_missing_plan_is_a_miss_and_run_list_is_not_repeated(self):
        api = Mock(
            side_effect=[
                {"artifacts": [artifact(), artifact()]},
                run(),
                {"artifacts": [artifact()]},
            ]
        )
        self.assertIsNone(self.discover(api).run_id)
        self.assertEqual(api.call_count, 3)

    def test_explicit_keyed_snapshot_without_valid_plan_fails_even_with_legacy(self):
        for invalid_plans in (
            [],
            [plan_artifact(expired=True)],
            [artifact(999, name="browse-comp-plus-plan-100")],
        ):
            with self.subTest(plans=invalid_plans):
                api = Mock(
                    side_effect=[
                        run(),
                        {
                            "artifacts": [
                                artifact(),
                                artifact(name=ARTIFACT_PREFIX + "100"),
                                *invalid_plans,
                            ]
                        },
                    ]
                )
                with self.assertRaisesRegex(
                    ArtifactDiscoveryError, "no unexpired plan"
                ):
                    self.discover(api, source_run_id="100")

    def test_run_artifact_api_error_fails_instead_of_treating_plan_as_missing(self):
        for explicit in (False, True):
            with self.subTest(explicit=explicit):
                responses = [run(), ArtifactDiscoveryError("GitHub API failure")]
                if not explicit:
                    responses.insert(0, {"artifacts": [artifact()]})
                with self.assertRaises(ArtifactDiscoveryError):
                    self.discover(
                        Mock(side_effect=responses),
                        source_run_id="100" if explicit else "",
                    )

    def test_explicit_missing_expired_or_wrong_run_artifacts_fail(self):
        for records in (
            [],
            [artifact(name=ARTIFACT_PREFIX + "100", expired=True)],
            [artifact(name=NAME + "-other")],
            [artifact(999, name=ARTIFACT_PREFIX + "100")],
        ):
            with self.subTest(records=records):
                api = Mock(side_effect=[run(), {"artifacts": records}])
                with self.assertRaisesRegex(ArtifactDiscoveryError, "no unexpired"):
                    self.discover(api, source_run_id="100")

    def test_explicit_untrusted_run_fails_before_listing_artifacts(self):
        api = Mock(return_value=run(head_branch="main"))
        with self.assertRaisesRegex(ArtifactDiscoveryError, "not a trusted"):
            self.discover(api, source_run_id="100")
        api.assert_called_once()

    def test_current_or_invalid_explicit_run_fails_without_api_calls(self):
        for run_id in ("200", "0", "-1", "abc", "100\n"):
            with self.subTest(run_id=run_id):
                api = Mock()
                with self.assertRaises(ArtifactDiscoveryError):
                    self.discover(api, source_run_id=run_id)
                api.assert_not_called()

    def test_empty_explicit_run_uses_automatic_discovery(self):
        api = Mock(return_value={"artifacts": []})
        self.assertIsNone(self.discover(api, source_run_id="").run_id)

    def test_invalid_context_or_key_fails_before_api_calls(self):
        for updates in (
            {"key": "key\nfound=true"},
            {"repository": "owner/repo?token=value"},
            {"branch": ""},
            {"current_run_id": ""},
        ):
            with self.subTest(updates=updates):
                api = Mock()
                with self.assertRaises(ArtifactDiscoveryError):
                    self.discover(api, **updates)
                api.assert_not_called()

    def test_api_failure_does_not_return_a_miss(self):
        api = Mock(side_effect=ArtifactDiscoveryError("GitHub API failure"))
        with self.assertRaises(ArtifactDiscoveryError):
            self.discover(api)

    def test_malformed_list_metadata_fails(self):
        for data in ({}, {"artifacts": None}, {"artifacts": ["bad"]}):
            with (
                self.subTest(data=data),
                self.assertRaisesRegex(ArtifactDiscoveryError, "invalid metadata"),
            ):
                self.discover(Mock(return_value=data))


class ArtifactAPITests(unittest.TestCase):
    def test_auth_is_in_header_not_url_and_response_is_parsed(self):
        response = Mock()
        response.read.return_value = b'{"artifacts": []}'
        response.__enter__ = Mock(return_value=response)
        response.__exit__ = Mock(return_value=None)
        with patch(
            "zg_bench.ci_prepared_artifact.urlopen", return_value=response
        ) as get:
            self.assertEqual(
                GitHubAPI("private-token")("/repos/o/r/actions/artifacts"),
                {"artifacts": []},
            )
        request = get.call_args.args[0]
        self.assertEqual(
            request.full_url, "https://api.github.com/repos/o/r/actions/artifacts"
        )
        self.assertEqual(request.get_header("Authorization"), "Bearer private-token")

    def test_http_and_network_errors_do_not_expose_private_details(self):
        private_url = "https://storage.invalid/?signature=private-token"
        for error in (
            HTTPError(
                private_url, 403, "private-token", {}, io.BytesIO(b"private-token")
            ),
            URLError(private_url),
        ):
            with self.subTest(kind=type(error).__name__):
                with (
                    patch("zg_bench.ci_prepared_artifact.urlopen", side_effect=error),
                    self.assertRaises(ArtifactDiscoveryError) as raised,
                ):
                    GitHubAPI("private-token")("/repos/o/r/actions/artifacts")
                self.assertNotIn("private-token", str(raised.exception))
                self.assertNotIn("storage.invalid", str(raised.exception))


class ArtifactCLITests(unittest.TestCase):
    def environment(self, **updates):
        return {
            "GH_TOKEN": "private-token",
            "GITHUB_REPOSITORY": REPOSITORY,
            "GITHUB_REF_NAME": BRANCH,
            "GITHUB_RUN_ID": "200",
            **updates,
        }

    def test_cli_appends_machine_outputs_to_github_output(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "outputs"
            output.write_text("existing=value\n", encoding="utf-8")
            api = Mock(
                side_effect=[{"artifacts": [artifact()]}, run(), prepared_artifacts()]
            )
            with (
                patch.dict(
                    "os.environ",
                    self.environment(GITHUB_OUTPUT=str(output)),
                    clear=True,
                ),
                patch("sys.argv", ["ci_prepared_artifact", "discover", "--key", KEY]),
                patch("zg_bench.ci_prepared_artifact.GitHubAPI", return_value=api),
                redirect_stdout(io.StringIO()) as stdout,
            ):
                self.assertEqual(main(), 0)
            self.assertEqual(stdout.getvalue(), "")
            self.assertEqual(
                output.read_text(encoding="utf-8"),
                "existing=value\nfound=true\nrun-id=100\n"
                f"artifact-name={NAME}\nlegacy=false\n",
            )

    def test_cli_returns_json_without_output_file(self):
        with (
            patch.dict("os.environ", self.environment(), clear=True),
            patch("sys.argv", ["ci_prepared_artifact", "discover", "--key", KEY]),
            patch(
                "zg_bench.ci_prepared_artifact.GitHubAPI",
                return_value=Mock(return_value={"artifacts": []}),
            ),
            redirect_stdout(io.StringIO()) as stdout,
        ):
            self.assertEqual(main(), 0)
        self.assertEqual(
            json.loads(stdout.getvalue()),
            {"found": False, "run-id": None, "artifact-name": "", "legacy": False},
        )

    def test_cli_failure_emits_no_miss_output_or_private_details(self):
        with (
            patch.dict("os.environ", self.environment(GH_TOKEN=""), clear=True),
            patch("sys.argv", ["ci_prepared_artifact", "discover", "--key", KEY]),
            redirect_stdout(io.StringIO()) as stdout,
            redirect_stderr(io.StringIO()) as stderr,
        ):
            self.assertEqual(main(), 1)
        self.assertEqual(stdout.getvalue(), "")
        self.assertIn("GH_TOKEN is required", stderr.getvalue())
        self.assertNotIn("private-token", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
