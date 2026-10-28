"""Deterministic length-stratified samples; input bytes remain unmodified."""

import argparse
import hashlib
import json
import shutil
import subprocess
from pathlib import Path

SEED = "browsecomp-index-estimate-v1"


def select(rows, count, seed=SEED):
    ordered = sorted(rows, key=lambda row: (row["bytes"], row["path"]))
    assert 0 < count <= len(rows)
    target = sum(row["bytes"] for row in rows) * count / len(rows)
    # One draw from each equal-count length stratum. Calibrate total bytes by
    # choosing among 128 predeclared seed offsets, before measuring any runtime.
    # This is byte calibration, not selection based on indexing performance.
    best = None
    for attempt in range(128):
        chosen = []
        for index in range(count):
            bucket = ordered[
                index * len(rows) // count : (index + 1) * len(rows) // count
            ]
            chosen.append(
                min(
                    bucket,
                    key=lambda row: hashlib.sha256(
                        f"{seed}\0{count}\0{attempt}\0{row['path']}".encode()
                    ).digest(),
                )
            )
        error = abs(sum(row["bytes"] for row in chosen) - target)
        if best is None or error < best[0]:
            best = (error, attempt, chosen)
    return best[2], best[1]


def distribution(rows):
    lengths = sorted(row["bytes"] for row in rows)
    return {
        "count": len(rows),
        "bytes": sum(lengths),
        "mean_bytes": sum(lengths) / len(rows),
        "length_bytes": {
            str(q): lengths[min(len(rows) - 1, int(q * (len(rows) - 1)))]
            for q in [0, 0.5, 0.9, 0.95, 0.99, 1]
        },
    }


def main(prepared, output, counts):
    rows = [
        json.loads(line)
        for line in (prepared / "corpus-manifest.jsonl").read_text().splitlines()
    ]
    assert len(rows) == 100195
    assert sum(row["bytes"] for row in rows) == 3247640161
    assert len({row["path"] for row in rows}) == len(rows)
    output.mkdir(parents=True, exist_ok=False)
    manifest = {
        "schema_version": 1,
        "seed": SEED,
        "algorithm": "equal-count length strata; minimum total-byte error among 128 seeded draws",
        "full_corpus": distribution(rows),
        "samples": {},
    }
    for count in counts:
        chosen, attempt = select(rows, count)
        root = output / str(count)
        root.mkdir()
        for row in chosen:
            raw = (prepared / "corpus" / row["path"]).read_bytes()
            assert len(raw) == row["bytes"]
            assert hashlib.sha256(raw).hexdigest() == row["sha256"]
            shutil.copyfile(prepared / "corpus" / row["path"], root / row["path"])
        subprocess.run(["git", "init", "--quiet", str(root)], check=True)
        subprocess.run(
            [
                "git",
                "-C",
                str(root),
                "-c",
                "core.hooksPath=/dev/null",
                "add",
                "--",
                "*.md",
            ],
            check=True,
        )
        manifest["samples"][str(count)] = {
            **distribution(chosen),
            "seed_offset": attempt,
            "byte_share_error_percent": 100
            * (sum(r["bytes"] for r in chosen) / (3247640161 * count / 100195) - 1),
            "files": sorted(chosen, key=lambda row: row["path"]),
        }
    (output / "samples.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(
        json.dumps(
            {
                key: {k: v for k, v in row.items() if k != "files"}
                for key, row in manifest["samples"].items()
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prepared", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--counts", default="256,1024")
    args = parser.parse_args()
    main(
        args.prepared.resolve(),
        args.output.resolve(),
        [int(n) for n in args.counts.split(",")],
    )
