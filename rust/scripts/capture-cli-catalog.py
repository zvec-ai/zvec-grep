#!/usr/bin/env python3
"""Capture real CLI help, management, and validation output in a pseudoterminal.

Uses only Python's standard library. All workspace and integration writes are
confined to a newly created temporary directory. HOME and CODEX_HOME are never
overridden. Global configuration success and integration upgrades are deliberately
excluded because those commands do not offer a complete scoped target override.
Pass --include-daemon to exercise a temporary loopback daemon, stopped in finally.
No model downloads, package installation, credentials, or grants are performed.
"""

import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import shlex
import socket
import struct
import subprocess
import tempfile
import termios
import time


HELP_TOPICS = (
    "search", "index", "status", "config", "auth", "server", "install",
    "uninstall", "help", "version", "models", "file-types", "environment", "env",
)


def read_available(fd, destination, timeout=0.02):
    readable, _, _ = select.select([fd], [], [], timeout)
    if not readable:
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


class Catalog:
    def __init__(self, binary, directory):
        self.binary = binary
        self.directory = directory
        self.workspace = directory / "workspace"
        self.workspace.mkdir()
        self.runtime = directory / "daemon"
        self.records = []
        self.environment = dict(os.environ)
        for key in tuple(self.environment):
            if key.startswith("ZVEC_GREP_") or key in ("NO_COLOR", "RUST_LOG"):
                self.environment.pop(key, None)
        self.environment.update({
            "TERM": "xterm-256color",
            "COLUMNS": "110",
            "LINES": "35",
            "ZVEC_GREP_HOME": str(self.runtime),
            "ZVEC_GREP_MODEL_CACHE": str(directory / "models"),
        })

    def capture(self, identifier, title, arguments, expected, covered, overrides=None,
                cancel_at=None, cancel_keys=b"\x1b"):
        master, slave = pty.openpty()
        before = termios.tcgetattr(slave)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 35, 110, 0, 0))
        os.set_blocking(master, False)
        environment = dict(self.environment)
        environment.update(overrides or {})
        collected = bytearray()
        snapshot = None
        child = subprocess.Popen(
            [str(self.binary), *arguments], cwd=self.workspace, env=environment,
            stdin=slave, stdout=slave, stderr=slave, close_fds=True,
        )
        try:
            deadline = time.monotonic() + 30
            while child.poll() is None:
                read_available(master, collected)
                if cancel_at and snapshot is None and cancel_at.encode() in collected:
                    read_available(master, collected, 0.03)
                    snapshot = bytes(collected)
                    os.write(master, cancel_keys)
                if time.monotonic() > deadline:
                    raise RuntimeError(f"{identifier}: unexpected prompt or timeout")
            read_available(master, collected)
            restored = before == termios.tcgetattr(slave)
            record = {
                "id": "cli-" + identifier,
                "title": title,
                "command": "zg " + shlex.join(arguments),
                "ansi": (snapshot if snapshot is not None else collected).decode("utf-8"),
                "exitCode": child.returncode,
                "provenance": "real PTY",
                "coveredCommands": covered,
                "columns": 110,
                "rows": 35,
                "terminalRestored": restored,
            }
            if overrides:
                record["environment"] = overrides
            if cancel_at:
                record["input"] = (
                    "Escape only; no operation confirmed" if cancel_keys == b"\x1b"
                    else "Explicit none; no operation confirmed"
                )
            self.records.append(record)
            if child.returncode != expected or not restored:
                raise RuntimeError(
                    f"{identifier}: expected exit {expected}, got {child.returncode}; "
                    f"terminal restored={restored}\n{record['ansi']}"
                )
            return record
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            os.close(master)
            os.close(slave)

    def collect_static(self):
        self.capture("no-arguments", "Usage with no arguments", [], 0, ["zg"])
        self.capture("help-alias", "Short help alias", ["-h"], 0, ["-h"])
        self.capture("version-alias", "Short version alias", ["-v"], 0, ["-v"])
        self.capture("help", "CLI help", ["--help"], 0, ["--help", "-h"])
        for topic in HELP_TOPICS:
            self.capture(f"help-{topic}", f"Help: {topic}", ["--help", topic], 0,
                         [f"--help {topic}"])
        self.capture("version", "Installed version", ["--version"], 0,
                     ["--version", "-v"])
        for identifier, title, arguments, code, covered in [
            ("bad-argument", "Unknown command option", ["--not-a-zg-option"], 1,
             ["query argument validation"]),
            ("bad-help-topic", "Unknown help topic", ["--help", "not-a-topic"], 1,
             ["--help invalid topic"]),
            ("drop-empty-index", "Drop an absent index",
             ["--index", "--drop", "--mode", "direct"], 0, ["--index --drop"]),
            ("auth-invalid-scope", "Invalid authorization scope",
             ["--auth", "grant", "--capability", "embedding", "--scope", "account"],
             2, ["--auth grant validation"]),
            ("config-invalid-model", "Unsupported model reference",
             ["--config", "model", "set", "not-a-model", "--default"], 1,
             ["--config model set validation"]),
            ("config-invalid-provider", "Unsupported provider",
             ["--config", "provider", "set", "not-a-provider", "--api-key", "DEMO_NOT_A_CREDENTIAL"],
             1, ["--config provider set validation"]),
            ("auth-status-empty", "Workspace has no remote data grant",
             ["--auth", "status"], 0, ["--auth status"]),
            ("auth-revoke-empty", "No remote data grant to revoke",
             ["--auth", "revoke"], 0, ["--auth revoke empty"]),
            ("index-not-ready", "Index readiness check fails in an empty workspace",
             ["--status", "--mode", "direct", "--check-ready"], 1,
             ["--status --check-ready failure"]),
        ]:
            self.capture(identifier, title, arguments, code, covered)

    def collect_integrations(self):
        self.capture("uninstall-picker", "Choose integrations to remove (cancelled)",
                     ["--uninstall"], 0, ["--uninstall picker"], cancel_at="Esc Cancel")
        # Blank input accepts detected defaults. The explicit `none` token is
        # the production fallback's cancellation choice and never installs.
        self.capture("install-line-picker", "Plain-terminal integration picker (cancelled)",
                     ["--install"], 0, ["--install line input fallback", "--uninstall shared line input fallback"],
                     {"TERM": "dumb"}, cancel_at="Agents [", cancel_keys=b"none\n")
        configuration = self.directory / "opencode" / "opencode.json"
        configuration.parent.mkdir()
        overrides = {
            "OPENCODE_CONFIG": str(configuration),
            "ZVEC_GREP_INSTALL_SKIP_SERVER": "1",
        }
        self.capture("install-success", "Integration configured in a temporary directory",
                     ["--install", "--target", "opencode", "--yes"], 0,
                     ["--install success"], overrides)
        assert json.loads(configuration.read_text())["mcp"]["zvec_grep"]
        self.capture("uninstall-success", "Temporary integration removed",
                     ["--uninstall", "--target", "opencode", "--yes"], 0,
                     ["--uninstall success"], overrides)
        assert "zvec_grep" not in json.loads(configuration.read_text()).get("mcp", {})
        invalid = "{ deliberately invalid integration configuration\n"
        configuration.write_text(invalid)
        self.capture("install-invalid-config", "Installation rejects malformed configuration",
                     ["--install", "--target", "opencode", "--yes"], 1,
                     ["--install failure"], overrides)
        assert configuration.read_text() == invalid
        self.capture("uninstall-invalid-config", "Removal rejects malformed configuration",
                     ["--uninstall", "--target", "opencode", "--yes"], 1,
                     ["--uninstall failure"], overrides)
        assert configuration.read_text() == invalid

    def collect_server(self, include_daemon):
        home = ["--home", str(self.runtime)]
        self.capture("server-stopped", "Resident server is stopped",
                     ["--server", "status", *home], 0, ["--server status stopped"])
        self.capture("server-not-ready", "Readiness check fails for a stopped server",
                     ["--server", "status", *home, "--check-ready"], 1,
                     ["--server status --check-ready failure"])
        if not include_daemon:
            return False
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        try:
            self.capture("server-started", "Temporary loopback server started",
                         ["--server", "on", *home, "--listen", f"127.0.0.1:{port}"], 0,
                         ["--server on"])
            self.capture("server-ready", "Temporary server is ready",
                         ["--server", "status", *home, "--check-ready"], 0,
                         ["--server status ready", "--server status --check-ready success"])
        finally:
            self.capture("server-stopped-after-run", "Temporary server stopped",
                         ["--server", "off", *home], 0, ["--server off"])
        self.capture("server-stop-verified", "Temporary daemon shutdown verified",
                     ["--server", "status", *home, "--check-ready"], 1,
                     ["--server shutdown verification"])
        return True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path,
                        default=Path(__file__).resolve().parents[1] / "target/debug/zg")
    parser.add_argument("--output", type=Path,
                        default=Path(tempfile.gettempdir()) / "zg-cli-command-captures.json")
    parser.add_argument("--include-daemon", action="store_true")
    options = parser.parse_args()
    binary = options.binary.resolve(strict=True)
    directory = Path(tempfile.mkdtemp(prefix="zg-cli-catalog-"))
    catalog = Catalog(binary, directory)
    metadata = {
        "fixtureDirectory": str(directory),
        "binary": str(binary),
        "configurationIsolation": "HOME and CODEX_HOME unchanged; only temporary OPENCODE_CONFIG written",
        "skipped": [
            {"coveredCommands": ["--install --upgrade-installed", "--install --upgrade-installed --dry-run"],
             "reason": "Scans all known user configurations; no complete scoped override. Not executed against real user settings."},
            {"coveredCommands": ["--config provider set success", "--config model set success"],
             "reason": "Writes global HOME configuration with no scoped target override. Validation failures are captured instead."},
            {"coveredCommands": ["--server --stdio", "--server run"],
             "reason": "Protocol or daemon process contract; not a human UI screen. Not represented by a fabricated screenshot."},
        ],
    }
    try:
        catalog.collect_static()
        catalog.collect_integrations()
        metadata["temporaryDaemonStopped"] = catalog.collect_server(options.include_daemon)
        metadata["modelsNotDownloaded"] = not (directory / "models").exists()
        assert metadata["modelsNotDownloaded"]
    finally:
        options.output.parent.mkdir(parents=True, exist_ok=True)
        options.output.write_text(json.dumps(catalog.records, ensure_ascii=False, indent=2) + "\n")
        options.output.with_suffix(".metadata.json").write_text(
            json.dumps(metadata, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({
        "output": str(options.output), "captureCount": len(catalog.records),
        "fixtureDirectory": str(directory),
        "temporaryDaemonStopped": metadata.get("temporaryDaemonStopped", False),
    }, indent=2))


if __name__ == "__main__":
    main()
