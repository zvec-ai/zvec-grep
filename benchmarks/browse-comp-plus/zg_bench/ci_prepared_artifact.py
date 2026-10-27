"""Find an exact prepared snapshot artifact without changing its archive identity."""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from collections.abc import Callable, Iterator
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode
from urllib.request import Request, urlopen

ARTIFACT_PREFIX = "browse-comp-plus-prepared-"
WORKFLOW_PATHS = frozenset(
    {".github/workflows/zg-bench.yml", ".github/workflows/browse-comp-plus.yml"}
)
PAGE_SIZE = 100
MAX_PAGES = 10
MAX_RESPONSE_BYTES = 4 * 1024 * 1024


class ArtifactDiscoveryError(RuntimeError):
    """A discovery failure that must not become an expensive preparation miss."""


class GitHubAPI:
    def __init__(self, token: str):
        if not token.strip():
            raise ArtifactDiscoveryError("GH_TOKEN is required for artifact discovery")
        self.token = token

    def __call__(self, path: str) -> dict[str, Any]:
        request = Request(
            "https://api.github.com" + path,
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {self.token}",
                "X-GitHub-Api-Version": "2022-11-28",
                "User-Agent": "zvec-grep-browsecomp-plus-ci",
            },
        )
        try:
            with urlopen(request, timeout=30) as response:
                payload = response.read(MAX_RESPONSE_BYTES + 1)
        except HTTPError as error:
            # Never print response bodies, credentials, or signed download URLs.
            raise ArtifactDiscoveryError(
                f"GitHub artifact API request failed (HTTP {error.code})"
            ) from None
        except (URLError, OSError, ValueError):
            raise ArtifactDiscoveryError("GitHub artifact API request failed") from None
        if len(payload) > MAX_RESPONSE_BYTES:
            raise ArtifactDiscoveryError("GitHub artifact API response is too large")
        try:
            data = json.loads(payload)
        except (ValueError, UnicodeError):
            raise ArtifactDiscoveryError(
                "GitHub artifact API returned invalid JSON"
            ) from None
        if not isinstance(data, dict):
            raise ArtifactDiscoveryError(
                "GitHub artifact API returned invalid metadata"
            )
        return data


@dataclass(frozen=True)
class Discovery:
    run_id: int | None = None
    artifact_name: str = ""
    legacy: bool = False

    def outputs(self) -> str:
        return (
            f"found={str(self.run_id is not None).lower()}\n"
            f"run-id={self.run_id or ''}\n"
            f"artifact-name={self.artifact_name}\n"
            f"legacy={str(self.legacy).lower()}\n"
        )


def _positive_id(value: Any) -> int | None:
    if isinstance(value, bool) or not re.fullmatch(r"[1-9][0-9]*", str(value)):
        return None
    return int(value)


def _artifacts(
    api: Callable[[str], dict[str, Any]], endpoint: str, *, name: str | None = None
) -> Iterator[dict[str, Any]]:
    for page in range(1, MAX_PAGES + 1):
        query: dict[str, str | int] = {"per_page": PAGE_SIZE, "page": page}
        if name is not None:
            query["name"] = name
        data = api(f"{endpoint}?{urlencode(query)}")
        records = data.get("artifacts")
        if not isinstance(records, list) or any(
            not isinstance(record, dict) for record in records
        ):
            raise ArtifactDiscoveryError(
                "GitHub artifact API returned invalid metadata"
            )
        yield from records
        if len(records) < PAGE_SIZE:
            return
    raise ArtifactDiscoveryError(
        "GitHub artifact discovery exceeded its pagination limit"
    )


def _matching_run(
    run: dict[str, Any], *, run_id: int, repository: str, branch: str
) -> bool:
    own_repository = run.get("repository")
    head_repository = run.get("head_repository")
    return (
        _positive_id(run.get("id")) == run_id
        and isinstance(own_repository, dict)
        and str(own_repository.get("full_name", "")).casefold() == repository.casefold()
        and isinstance(head_repository, dict)
        and str(head_repository.get("full_name", "")).casefold()
        == repository.casefold()
        and run.get("head_branch") == branch
        and run.get("event") == "workflow_dispatch"
        and run.get("path") in WORKFLOW_PATHS
    )


def _run_artifact_names(
    api: Callable[[str], dict[str, Any]], endpoint: str, run_id: int
) -> set[str]:
    names = set()
    for artifact in _artifacts(api, f"{endpoint}/runs/{run_id}/artifacts"):
        workflow_run = artifact.get("workflow_run")
        name = artifact.get("name")
        if (
            artifact.get("expired") is False
            and isinstance(workflow_run, dict)
            and _positive_id(workflow_run.get("id")) == run_id
            and isinstance(name, str)
        ):
            names.add(name)
    return names


def discover(
    api: Callable[[str], dict[str, Any]],
    *,
    key: str,
    repository: str,
    branch: str,
    current_run_id: str,
    source_run_id: str = "",
) -> Discovery:
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,199}", key):
        raise ArtifactDiscoveryError("invalid preparation artifact key")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ArtifactDiscoveryError("GITHUB_REPOSITORY is required and must be valid")
    if not branch or any(character in branch for character in "\r\n\0"):
        raise ArtifactDiscoveryError("GITHUB_REF_NAME is required and must be valid")
    current_id = _positive_id(current_run_id)
    if current_id is None:
        raise ArtifactDiscoveryError("GITHUB_RUN_ID is required and must be valid")
    requested_id = _positive_id(source_run_id) if source_run_id else None
    if source_run_id and requested_id is None:
        raise ArtifactDiscoveryError("source run ID must be a positive integer")
    if requested_id == current_id:
        raise ArtifactDiscoveryError("source run must differ from the current run")

    endpoint = f"/repos/{repository}/actions"
    expected_name = ARTIFACT_PREFIX + key
    if requested_id is not None:
        run = api(f"{endpoint}/runs/{requested_id}")
        if not _matching_run(
            run, run_id=requested_id, repository=repository, branch=branch
        ):
            raise ArtifactDiscoveryError(
                "requested source run is not a trusted workflow dispatch on this branch"
            )
        names = _run_artifact_names(api, endpoint, requested_id)
        if expected_name in names:
            if f"browse-comp-plus-plan-{requested_id}" not in names:
                raise ArtifactDiscoveryError(
                    "requested keyed snapshot has no unexpired plan artifact from the same run"
                )
            return Discovery(requested_id, expected_name)
        legacy_name = ARTIFACT_PREFIX + str(requested_id)
        if legacy_name in names:
            return Discovery(requested_id, legacy_name, legacy=True)
        raise ArtifactDiscoveryError(
            "requested source run has no unexpired matching prepared snapshot artifact"
        )

    checked_runs: dict[int, bool] = {}
    for artifact in _artifacts(api, f"{endpoint}/artifacts", name=expected_name):
        if (
            artifact.get("name") != expected_name
            or artifact.get("expired") is not False
        ):
            continue
        workflow_run = artifact.get("workflow_run")
        if not isinstance(workflow_run, dict):
            continue
        run_id = _positive_id(workflow_run.get("id"))
        if run_id is None or run_id == current_id:
            continue
        if workflow_run.get("head_branch") != branch:
            continue
        if run_id not in checked_runs:
            trusted = _matching_run(
                api(f"{endpoint}/runs/{run_id}"),
                run_id=run_id,
                repository=repository,
                branch=branch,
            )
            names = _run_artifact_names(api, endpoint, run_id) if trusted else set()
            checked_runs[run_id] = (
                expected_name in names and f"browse-comp-plus-plan-{run_id}" in names
            )
        if checked_runs[run_id]:
            return Discovery(run_id, expected_name)
    return Discovery()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("discover",))
    parser.add_argument("--key", required=True)
    parser.add_argument("--source-run-id", default="")
    args = parser.parse_args()
    try:
        result = discover(
            GitHubAPI(os.environ.get("GH_TOKEN", "")),
            key=args.key,
            repository=os.environ.get("GITHUB_REPOSITORY", ""),
            branch=os.environ.get("GITHUB_REF_NAME", ""),
            current_run_id=os.environ.get("GITHUB_RUN_ID", ""),
            source_run_id=args.source_run_id,
        )
        output = os.environ.get("GITHUB_OUTPUT")
        if output:
            with Path(output).open("a", encoding="utf-8") as destination:
                destination.write(result.outputs())
        else:
            print(
                json.dumps(
                    {
                        "found": result.run_id is not None,
                        "run-id": result.run_id,
                        "artifact-name": result.artifact_name,
                        "legacy": result.legacy,
                    }
                )
            )
    except ArtifactDiscoveryError as error:
        print(f"Prepared snapshot discovery failed: {error}", file=sys.stderr)
        return 1
    except OSError:
        print("Could not write prepared snapshot discovery outputs", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
