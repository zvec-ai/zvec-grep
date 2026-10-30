"""Linux external index measurement; never changes or instruments the product."""

import argparse
import json
import os
import signal
import subprocess
import threading
import time
from pathlib import Path


def sample_tree(root_pid):
    table = {}
    for path in Path("/proc").iterdir():
        if not path.name.isdigit():
            continue
        try:
            fields = (path / "stat").read_text().rsplit(") ", 1)[1].split()
            table[int(path.name)] = (
                int(fields[1]),
                int(fields[19]),
                int(fields[21]) * os.sysconf("SC_PAGE_SIZE"),
            )
        except (OSError, ValueError, IndexError):
            continue
    pids = {root_pid}
    while True:
        children = {pid for pid, row in table.items() if row[0] in pids}
        expanded = pids | children
        if expanded == pids:
            break
        pids = expanded
    selected = {pid: table[pid] for pid in pids if pid in table}
    io = {}
    for pid, row in selected.items():
        try:
            io[f"{pid}:{row[1]}"] = {
                k: int(v)
                for k, v in (
                    line.split(": ")
                    for line in (Path("/proc") / str(pid) / "io")
                    .read_text()
                    .splitlines()
                )
            }
        except (OSError, ValueError):
            pass
    return sum(row[2] for row in selected.values()), io, sorted(selected)


def measure(command, cwd, output, timeout):
    assert Path("/proc").is_dir(), "measurement requires Linux /proc"
    output.mkdir(parents=True, exist_ok=False)
    samples, io_maxima, progress = [], {}, []
    lock = threading.Lock()
    started = time.monotonic()
    child = subprocess.Popen(
        command,
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )

    def capture(stream, name):
        with (
            (output / f"{name}.log").open("wb") as raw,
            (output / f"{name}.jsonl").open("w") as events,
        ):
            for line in iter(stream.readline, b""):
                elapsed = time.monotonic() - started
                raw.write(line)
                raw.flush()
                text = line.decode("utf-8", errors="replace").rstrip("\r\n")
                events.write(
                    json.dumps({"elapsed_seconds": elapsed, "text": text}) + "\n"
                )
                events.flush()
                with lock:
                    progress.append({"elapsed_seconds": elapsed, "text": text})

    threads = [
        threading.Thread(target=capture, args=(child.stdout, "stdout")),
        threading.Thread(target=capture, args=(child.stderr, "stderr")),
    ]
    for thread in threads:
        thread.start()
    timed_out, next_sample, next_log = False, 0, 15
    try:
        while True:
            elapsed = time.monotonic() - started
            if elapsed >= next_sample:
                rss, counters, pids = sample_tree(child.pid)
                if rss:
                    samples.append(
                        {"elapsed_seconds": elapsed, "rss_bytes": rss, "pids": pids}
                    )
                for key, values in counters.items():
                    previous = io_maxima.setdefault(key, {})
                    for name, value in values.items():
                        previous[name] = max(previous.get(name, 0), value)
                next_sample = elapsed + 0.5
            if elapsed >= next_log:
                with lock:
                    latest = (
                        progress[-1]["text"]
                        if progress
                        else "waiting for product output"
                    )
                print(f"Index elapsed={elapsed:.1f}s: {latest}", flush=True)
                next_log = elapsed + 15
            pid, status, usage = os.wait4(child.pid, os.WNOHANG)
            if pid:
                total_seconds = time.monotonic() - started
                child.returncode = os.waitstatus_to_exitcode(status)
                break
            if elapsed > timeout and not timed_out:
                timed_out = True
                os.killpg(child.pid, signal.SIGKILL)
            time.sleep(0.025)
    finally:
        # Direct mode should leave no daemons. Clean only this measurement's
        # process group if a product accidentally leaves descendants behind.
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        for thread in threads:
            thread.join(timeout=10)
    diagnostics = []
    for record in progress:
        if record["text"].startswith("Index diagnostics: "):
            diagnostics.append(
                json.loads(record["text"].removeprefix("Index diagnostics: "))
            )
    result = {
        "schema_version": 1,
        "command": command,
        "cwd": str(cwd),
        "total_seconds": total_seconds,
        "returncode": child.returncode,
        "timed_out": timed_out,
        "status": "success" if child.returncode == 0 and not timed_out else "failed",
        "scope": "CLI spawn through exit, including model load, writes and final optimization; downloads warmed separately",
        "rss": {
            "sample_interval_seconds": 0.5,
            "scope": "sum of product process tree RSS; shared pages may be counted more than once",
            "samples": samples,
            "peak_bytes": max((r["rss_bytes"] for r in samples), default=None),
        },
        "wait4": {
            "user_seconds": usage.ru_utime,
            "system_seconds": usage.ru_stime,
            "maximum_rss_kib": usage.ru_maxrss,
            "filesystem_input_blocks": usage.ru_inblock,
            "filesystem_output_blocks": usage.ru_oublock,
            "block_bytes": 512,
            "scope": "wait4 resource usage for direct CLI and descendants it reaped",
        },
        "proc_io": {
            "sampled_process_maxima": io_maxima,
            "scope": "sampled /proc/PID/io maxima; short-lived processes and final increments may be missed",
        },
        "public_index_diagnostics": diagnostics or None,
        "phase_timings": diagnostics[-1].get("timings") if diagnostics else None,
        "missing_phase_timings": None
        if diagnostics
        else "unavailable: product does not expose phase timings in its public debug output",
        "external_progress_markers": [
            r
            for r in progress
            if any(
                k in r["text"].lower() for k in ["preparing ", "finalizing", "optimiz"]
            )
        ],
        "progress_marker_scope": "external receipt times only, not exclusive embedding/write duration",
    }
    (output / "measurement.json").write_text(json.dumps(result, indent=2) + "\n")
    print(f"Index {result['status']}: {total_seconds:.3f}s", flush=True)
    return (
        child.returncode
        if child.returncode > 0
        else (1 if child.returncode or timed_out else 0)
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cwd", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--timeout", type=float, default=1200)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    assert command
    raise SystemExit(
        measure(command, args.cwd.resolve(), args.output.resolve(), args.timeout)
    )
