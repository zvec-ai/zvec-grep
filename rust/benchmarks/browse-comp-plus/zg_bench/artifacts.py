from __future__ import annotations

import hashlib
import json
import os
import re
import tempfile
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Iterable


def utc_now() -> str:
    return datetime.now(UTC).isoformat()


def new_run_id() -> str:
    return datetime.now(UTC).strftime("%Y%m%d-%H%M%S")


def find_run(artifacts: Path, run_id: str) -> Path:
    runs = artifacts / "runs"
    if runs.is_dir():
        for path in runs.iterdir():
            metadata_path = path / "run.json"
            if not (
                path.is_dir()
                and path.name == run_id
                and metadata_path.is_file()
            ):
                continue
            metadata = read_json(metadata_path)
            if isinstance(metadata, dict) and str(metadata.get("run_id")) == run_id:
                return path
    raise RuntimeError(f"benchmark run not found: {run_id}")


def atomic_write_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{path.name}.", suffix=".tmp", dir=path.parent
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            output.write(content)
            output.flush()
            os.fsync(output.fileno())
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)


def write_json(path: Path, value: Any) -> None:
    atomic_write_text(
        path, json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n"
    )


def append_jsonl(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as output:
        output.write(json.dumps(value, ensure_ascii=False) + "\n")
        output.flush()
        os.fsync(output.fileno())


def read_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def read_jsonl(path: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    with path.open(encoding="utf-8") as source:
        for number, line in enumerate(source, 1):
            if not line.strip():
                continue
            value = json.loads(line)
            if not isinstance(value, dict):
                raise ValueError(f"{path}:{number}: expected a JSON object")
            rows.append(value)
    return rows


def sha256_file(path: Path, chunk_size: int = 1024 * 1024) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(chunk_size):
            digest.update(chunk)
    return digest.hexdigest()


def fingerprint(parts: Iterable[str]) -> str:
    digest = hashlib.sha256()
    for part in parts:
        digest.update(part.encode("utf-8"))
        digest.update(b"\0")
    return digest.hexdigest()


def next_attempt_number(root: Path) -> int:
    pattern = re.compile(r"attempt-(\d+)")
    numbers = [
        int(match.group(1))
        for path in root.glob("attempt-*")
        if path.is_dir() and (match := pattern.fullmatch(path.name))
    ]
    return max(numbers, default=0) + 1
