from __future__ import annotations

import platform
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

from .runner import (
    Profile,
    normalize_zvec_grep_package,
    validate_profile_credentials,
    validate_zvec_grep_package_compatibility,
)
from .settings import (
    ZVEC_GREP_EMBEDDING,
    ZVEC_GREP_EMBEDDING_ENDPOINT,
    ZVEC_GREP_PACKAGE,
)


@dataclass(frozen=True)
class Check:
    name: str
    ok: bool
    detail: str
    required: bool = True


def _run_version(command: list[str]) -> tuple[bool, str]:
    try:
        completed = subprocess.run(
            command,
            check=False,
            capture_output=True,
            text=True,
            timeout=15,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return False, str(error)

    output = (completed.stdout or completed.stderr).strip()
    if completed.returncode != 0:
        return False, output or f"exited with status {completed.returncode}"
    return True, output


def collect_checks(
    *,
    agent: str | None = None,
    model: str | None = None,
    profiles: Sequence[Profile] = (),
    zvec_grep_package: str = ZVEC_GREP_PACKAGE,
    embedding_model: str = ZVEC_GREP_EMBEDDING,
    embedding_endpoint: str | None = ZVEC_GREP_EMBEDDING_ENDPOINT,
) -> list[Check]:
    version = sys.version_info
    python_ok = (3, 12) <= version[:2] < (3, 14)
    checks = [
        Check(
            "Python",
            python_ok,
            f"{version.major}.{version.minor}.{version.micro}",
        )
    ]

    harbor = shutil.which("harbor")
    if harbor is None:
        checks.append(Check("Harbor", False, "not found on PATH"))
    else:
        ok, detail = _run_version([harbor, "--version"])
        checks.append(Check("Harbor", ok, detail or harbor))

    docker = shutil.which("docker")
    if docker is None:
        checks.append(Check("Docker", False, "not found on PATH"))
        checks.append(
            Check(
                "Docker Compose",
                False,
                "Docker is not found on PATH",
            )
        )
    else:
        ok, detail = _run_version(
            [docker, "version", "--format", "{{.Server.Version}}"]
        )
        checks.append(
            Check(
                "Docker",
                ok,
                f"server {detail}" if ok else f"daemon unavailable: {detail}",
            )
        )
        compose_ok, compose_detail = _run_version(
            [docker, "compose", "version"]
        )
        compose_match = re.search(r"\bv?(\d+)\.\d+", compose_detail)
        compose_v2 = (
            compose_ok
            and compose_match is not None
            and int(compose_match.group(1)) >= 2
        )
        if compose_v2:
            checks.append(Check("Docker Compose", True, compose_detail))
        else:
            failure = compose_detail.splitlines()[0] if compose_detail else "unavailable"
            if compose_ok:
                failure += "; Docker Compose v2 is required"
            checks.append(
                Check(
                    "Docker Compose",
                    False,
                    f"{failure}; install the Docker Compose CLI plugin",
                )
            )

    machine = platform.machine() or "unknown"
    system = platform.system() or "unknown"
    note = f"{system} {machine}"
    native_linux_x86 = system == "Linux" and machine in {"x86_64", "amd64"}
    if system == "Darwin" and machine in {"arm64", "aarch64"}:
        note += "; some benchmark images may run through emulation"
    checks.append(Check("Platform", native_linux_x86, note, required=False))

    if agent is not None and model is not None:
        checks.extend(
            _collect_run_checks(
                agent=agent,
                model=model,
                profiles=profiles,
                zvec_grep_package=zvec_grep_package,
                embedding_model=embedding_model,
                embedding_endpoint=embedding_endpoint,
            )
        )
    return checks


def _collect_run_checks(
    *,
    agent: str,
    model: str,
    profiles: Sequence[Profile],
    zvec_grep_package: str,
    embedding_model: str = ZVEC_GREP_EMBEDDING,
    embedding_endpoint: str | None = ZVEC_GREP_EMBEDDING_ENDPOINT,
) -> list[Check]:
    checks: list[Check] = []
    profile_label = ", ".join(profiles) or "none"
    checks.append(Check("Run profiles", bool(profiles), profile_label))

    try:
        validate_profile_credentials(
            profiles,
            agent=agent,
            model=model,
            embedding_model=embedding_model,
            embedding_endpoint=embedding_endpoint,
        )
    except ValueError as error:
        checks.append(Check("Credentials", False, str(error)))
    else:
        checks.append(Check("Credentials", True, "required variables are set"))

    if "zvec-grep" not in profiles:
        return checks

    try:
        validate_zvec_grep_package_compatibility(
            profiles,
            agent=agent,
            zvec_grep_package=zvec_grep_package,
        )
    except ValueError as error:
        checks.append(Check("zvec-grep package", False, str(error)))
    else:
        checks.append(
            Check(
                "zvec-grep package",
                True,
                normalize_zvec_grep_package(zvec_grep_package),
            )
        )

    if embedding_model.startswith("qwen/"):
        checks.append(
            Check(
                "Remote Embedding",
                True,
                f"{embedding_model}; source content is sent remotely "
                "with a Workspace grant created during setup",
            )
        )

    source = _local_package_source(zvec_grep_package)
    if source is None or not source.is_dir():
        return checks

    package_json = source / "package.json"
    checks.append(
        Check(
            "Local package",
            package_json.is_file(),
            str(source.resolve()) if package_json.is_file() else f"missing {package_json}",
        )
    )

    node = shutil.which("node")
    if node is None:
        checks.append(Check("Node.js", False, "not found on PATH; Node.js >=22 is required"))
    else:
        ok, detail = _run_version([node, "--version"])
        match = re.search(r"v?(\d+)", detail)
        version_ok = ok and match is not None and int(match.group(1)) >= 22
        checks.append(
            Check(
                "Node.js",
                version_ok,
                detail if version_ok else f"{detail or 'unknown'}; Node.js >=22 is required",
            )
        )

    npm = shutil.which("npm")
    if npm is None:
        checks.append(Check("npm", False, "not found on PATH"))
    else:
        ok, detail = _run_version([npm, "--version"])
        checks.append(Check("npm", ok, detail or npm))
        registry_ok, registry = _run_version(
            [
                npm,
                "config",
                "get",
                "registry",
                "--location=project",
                "--prefix",
                str(source),
            ]
        )
        internal_registry = "registry.anpm.alibaba-inc.com" in registry
        if internal_registry:
            registry = (
                f"{registry}; configure https://registry.npmjs.org/ for this run "
                "or update the user-level npm registry"
            )
        checks.append(
            Check(
                "npm registry",
                registry_ok and not internal_registry,
                registry or "not configured",
                required=not internal_registry,
            )
        )

    tsc = source / "node_modules" / ".bin" / "tsc"
    checks.append(
        Check(
            "Local dependencies",
            tsc.is_file(),
            str(tsc) if tsc.is_file() else f"missing {tsc}; run 'npm ci' from {source}",
        )
    )

    lockfile = source / "package-lock.json"
    if lockfile.is_file():
        lock_text = lockfile.read_text(encoding="utf-8")
        private_registry = "registry.anpm.alibaba-inc.com" in lock_text
        checks.append(
            Check(
                "Package lock registry",
                not private_registry,
                "contains registry.anpm.alibaba-inc.com"
                if private_registry
                else "no Alibaba-internal registry URLs",
            )
        )
    else:
        checks.append(Check("Package lock registry", False, f"missing {lockfile}"))
    return checks


def _local_package_source(value: str) -> Path | None:
    candidate = Path(value).expanduser()
    if candidate.exists():
        return candidate.resolve()
    if value.startswith((".", "/", "~")):
        return candidate
    return None


def print_report(checks: list[Check]) -> int:
    for check in checks:
        marker = "OK" if check.ok else ("FAIL" if check.required else "WARN")
        print(f"[{marker}] {check.name}: {check.detail}")
    return 0 if all(check.ok for check in checks if check.required) else 1


def run_doctor(
    *,
    agent: str | None = None,
    model: str | None = None,
    profiles: Sequence[Profile] = (),
    zvec_grep_package: str = ZVEC_GREP_PACKAGE,
    embedding_model: str = ZVEC_GREP_EMBEDDING,
    embedding_endpoint: str | None = ZVEC_GREP_EMBEDDING_ENDPOINT,
) -> int:
    return print_report(
        collect_checks(
            agent=agent,
            model=model,
            profiles=profiles,
            zvec_grep_package=zvec_grep_package,
            embedding_model=embedding_model,
            embedding_endpoint=embedding_endpoint,
        )
    )
