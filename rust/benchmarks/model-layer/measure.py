#!/usr/bin/env python3
"""Run one benchmark process and sample its peak resident set size."""

from __future__ import annotations

import argparse
import subprocess
import sys
import time


def rss_bytes(pid: int) -> int | None:
    completed = subprocess.run(
        ["ps", "-o", "rss=", "-p", str(pid)],
        check=False,
        capture_output=True,
        text=True,
    )
    value = completed.stdout.strip()
    if not value:
        return None
    return int(value.splitlines()[0].strip()) * 1024


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("command", nargs=argparse.REMAINDER)
    arguments = parser.parse_args()
    command = arguments.command
    if command[:1] == ["--"]:
        command = command[1:]
    if not command:
        parser.error("a command is required after --")

    process = subprocess.Popen(command)
    peak = 0
    while process.poll() is None:
        current = rss_bytes(process.pid)
        if current is not None:
            peak = max(peak, current)
        time.sleep(0.01)
    current = rss_bytes(process.pid)
    if current is not None:
        peak = max(peak, current)
    return_code = process.wait()
    print(f"ZG_MODEL_BENCH_PEAK_RSS_BYTES={peak}")
    return return_code


if __name__ == "__main__":
    sys.exit(main())
