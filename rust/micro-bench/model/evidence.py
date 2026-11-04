"""Filesystem helpers for benchmark evidence and reports."""
import hashlib
import json


def digest(path):
    with path.open("rb") as stream:
        checksum = hashlib.sha256()
        for chunk in iter(lambda: stream.read(1024*1024), b""):
            checksum.update(chunk)
        return checksum.hexdigest()


def save(path, data):
    temporary = path.with_suffix(path.suffix+".tmp")
    temporary.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")
    temporary.replace(path)


def missing_round_evidence(folder, successful):
    # Streamed logs can be empty when a workload fails before issuing a batch.
    required = ("stdout.log", "stderr.log", "requests.jsonl", "gpu.jsonl", "background-scans.jsonl",
                "events.json", "resources.json", "gpu-phases.json", "background.json")
    missing = []
    for name in required:
        path = folder / name
        needs_content = name.endswith(".json") or (successful and name == "requests.jsonl")
        if not path.is_file() or (needs_content and path.stat().st_size == 0):
            missing.append(name)
    return missing


def publish_round(folder, result, events, samples, background):
    save(folder / "events.json", events)
    save(folder / "resources.json", samples)
    save(folder / "background.json", background)
    missing = missing_round_evidence(folder, result["protocol_pass"])
    if missing:
        raise RuntimeError(f"Incomplete round {folder}: missing evidence {', '.join(missing)}")
    save(folder / "result.json", result)
