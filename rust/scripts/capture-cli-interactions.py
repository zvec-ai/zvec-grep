#!/usr/bin/env python3
"""Capture production credential/consent prompts through a safe Rust example.

Build first: cargo build -p zg-cli --example interactive_presentation
This script invokes only the presentation example, never the actual configuration
or authorization operations. The synthetic credential is used only to check that
masking works. Each process is cancelled with Escape before a value is accepted.
"""

import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import struct
import subprocess
import tempfile
import termios
import time


DUMMY_CREDENTIAL = "DEMO_NOT_A_REAL_API_KEY"


def read_available(fd, destination, timeout=0.02):
    ready, _, _ = select.select([fd], [], [], timeout)
    if not ready:
        return
    while True:
        try:
            chunk = os.read(fd, 65536)
        except BlockingIOError:
            return
        except OSError as error:
            if error.errno == errno.EIO:
                return
            raise
        if not chunk:
            return
        destination.extend(chunk)


def capture_mode(binary, directory, mode, snapshots):
    master, slave = pty.openpty()
    before = termios.tcgetattr(slave)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 110, 0, 0))
    os.set_blocking(master, False)
    environment = dict(os.environ)
    for key in ("NO_COLOR", "RUST_LOG"):
        environment.pop(key, None)
    environment["TERM"] = "xterm-256color"
    if os.uname().sysname == "Darwin":
        previous = environment.get("DYLD_LIBRARY_PATH")
        environment["DYLD_LIBRARY_PATH"] = str(binary.parent.parent) + (
            os.pathsep + previous if previous else ""
        )
    arguments = ["--mode", mode]
    collected = bytearray()
    child = subprocess.Popen(
        [str(binary), *arguments], cwd=directory, env=environment,
        stdin=slave, stdout=slave, stderr=slave, close_fds=True,
    )
    records = []
    cursor = 0

    def await_marker(marker):
        nonlocal cursor
        expected = marker.encode()
        deadline = time.monotonic() + 10
        while expected not in collected[cursor:]:
            read_available(master, collected)
            if child.poll() is not None:
                raise RuntimeError(f"{mode}: exited before {marker!r}")
            if time.monotonic() > deadline:
                raise RuntimeError(f"{mode}: prompt deadline exceeded for {marker!r}")
        cursor = collected.index(expected, cursor) + len(expected)
        read_available(master, collected, 0.03)

    try:
        for identifier, title, keys, marker in snapshots:
            if keys:
                os.write(master, keys)
            await_marker(marker)
            ansi = collected.decode("utf-8")
            assert DUMMY_CREDENTIAL not in ansi, "credential leaked into terminal output"
            clear = list(re.finditer(r"\x1b\[(?:0)?J", ansi))
            if clear:
                ansi = ansi[clear[-1].end():]
            records.append({
                "id": identifier,
                "title": title,
                "command": "interactive_presentation " + shlex.join(arguments),
                "ansi": ansi,
                "provenance": "PTY renderer fixture",
                "coveredCommands": ["first-index " + mode + " prompt"],
                "fixtureNotice": "Production prompt with synthetic data; no credentials or grants are saved.",
                "columns": 110,
                "rows": 30,
            })
        os.write(master, b"\x1b")
        deadline = time.monotonic() + 5
        while child.poll() is None:
            read_available(master, collected)
            if time.monotonic() > deadline:
                raise RuntimeError(f"{mode}: Escape did not cancel")
        read_available(master, collected)
        restored = before == termios.tcgetattr(slave)
        assert child.returncode == 130
        assert restored
        assert b"\x1b[?25h" in collected
        assert b"Decision: cancelled" in collected
        assert DUMMY_CREDENTIAL.encode() not in collected
        for record in records:
            record["exitCode"] = child.returncode
            record["verification"] = {
                "terminalRestored": restored,
                "cursorRestored": True,
                "cancelled": True,
                "credentialNotDisplayed": True,
                "configurationOrGrantWrites": False,
            }
        return records
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        os.close(master)
        os.close(slave)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=(
        Path(__file__).resolve().parents[1] / "target/debug/examples/interactive_presentation"
    ))
    parser.add_argument("--output", type=Path, default=(
        Path(tempfile.gettempdir()) / "zg-cli-interactive-fixtures.json"
    ))
    arguments = parser.parse_args()
    binary = arguments.binary.resolve(strict=True)
    directory = Path(tempfile.mkdtemp(prefix="zg-cli-prompt-fixtures-"))
    records = capture_mode(binary, directory, "credential", [
        ("credential-empty", "Credential entry: hidden input", None, "Esc Cancel"),
        ("credential-validation", "Credential entry: empty value rejected", b"\r",
         "Enter an API Key to continue."),
        ("credential-masked", "Credential entry: synthetic value stays masked",
         b"\x1b[200~" + DUMMY_CREDENTIAL.encode() + b"\x1b[201~", "*" * len(DUMMY_CREDENTIAL)),
    ])
    records += capture_mode(binary, directory, "authorization", [
        ("authorization-once", "Remote data consent: scope and destination", None, "Esc Cancel"),
        ("authorization-workspace", "Remote data consent: workspace choice selected, not accepted",
         b"\x1b[B", "Esc Cancel"),
    ])
    assert not any(directory.iterdir()), "presentation fixture unexpectedly created files"
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(json.dumps(records, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({
        "output": str(arguments.output), "captureCount": len(records),
        "fixtureDirectory": str(directory), "fixtureDirectoryUnchanged": True,
    }, indent=2))


if __name__ == "__main__":
    main()
