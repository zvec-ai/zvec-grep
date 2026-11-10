"""The single pinned ripgrep intervention shared by every OpenCode arm."""
from __future__ import annotations

import json
import shlex

RIPGREP_VERSION = "14.1.1"
RIPGREP_PATH = "/usr/local/bin/rg"
RIPGREP_ARCHIVE = f"ripgrep-{RIPGREP_VERSION}-x86_64-unknown-linux-musl.tar.gz"
RIPGREP_ARCHIVE_URL = (
    f"https://github.com/BurntSushi/ripgrep/releases/download/{RIPGREP_VERSION}/"
    + RIPGREP_ARCHIVE
)
# Official release checksum and the rg member of that verified archive.
# Verified against the official .tar.gz.sha256 asset on 2026-10-27.
RIPGREP_ARCHIVE_SHA256 = "4cf9f2741e6c465ffdb7c26f38056a59e2a2544b51f7cc128ef28337eeae4d8e"
RIPGREP_BINARY_SHA256 = "f401154e2393f9002ac77e419f9ee5521c18f4f8cd3e32293972f493ba06fce7"
RIPGREP_PREFLIGHT_FILENAME = "ripgrep-preflight.json"
RIPGREP_SEARCH_OUTPUT = "2:rg-preflight-needle-42"


def _require_frozen_hashes() -> None:
    for digest in (RIPGREP_ARCHIVE_SHA256, RIPGREP_BINARY_SHA256):
        if len(digest) != 64 or any(c not in "0123456789abcdef" for c in digest):
            raise ValueError("ripgrep release hashes have not been verified and frozen")


def ripgrep_install_command() -> str:
    """Install into the task container, without changing the Agent's PATH."""
    _require_frozen_hashes()
    member = RIPGREP_ARCHIVE.removesuffix(".tar.gz") + "/rg"
    return (
        "set -euo pipefail; "
        'test "$(uname -s)" = Linux; test "$(uname -m)" = x86_64; '
        'rg_install_tmp="$(mktemp -d)"; '
        "trap 'rm -rf -- \"$rg_install_tmp\"' EXIT; "
        "curl --fail --location --silent --show-error "
        "--retry 3 --retry-all-errors --retry-delay 2 "
        "--connect-timeout 10 --max-time 90 --retry-max-time 180 "
        f"{shlex.quote(RIPGREP_ARCHIVE_URL)} "
        '--output "$rg_install_tmp/archive.tar.gz"; '
        f"printf '%s  %s\\n' {shlex.quote(RIPGREP_ARCHIVE_SHA256)} "
        '"$rg_install_tmp/archive.tar.gz" | sha256sum --check --status; '
        'tar -xzf "$rg_install_tmp/archive.tar.gz" -C "$rg_install_tmp" '
        f"--strip-components=1 {shlex.quote(member)}; "
        f"printf '%s  %s\\n' {shlex.quote(RIPGREP_BINARY_SHA256)} "
        '"$rg_install_tmp/rg" | sha256sum --check --status; '
        f'install -m 0755 "$rg_install_tmp/rg" {shlex.quote(RIPGREP_PATH)}; '
        f"{shlex.quote(RIPGREP_PATH)} --version"
    )


def ripgrep_preflight_command() -> str:
    """Use the same nvm initialization, user, cwd and PATH as OpenCode.run.

    The probe creates only a temporary fixture, outside the task repository.
    Its JSON includes only selected environment facts, never credentials.
    Validation occurs in Python so a failed probe is still preserved verbatim.
    """
    script = r"""
const fs = require('fs'), os = require('os'), path = require('path');
const crypto = require('crypto'), cp = require('child_process');
const result = {schema_version: 1, status: 'failed', uid: process.getuid(),
  gid: process.getgid(), user: os.userInfo().username, cwd: process.cwd(),
  path: process.env.PATH};
let fixture;
function run(file, args) {
  const value = cp.spawnSync(file, args, {encoding: 'utf8'});
  if (value.error || value.status !== 0) {
    const error = new Error('ripgrep probe command failed');
    error.command = [file, ...args]; error.exit_status = value.status;
    error.stderr = value.stderr || ''; throw error;
  }
  return value.stdout.trimEnd();
}
try {
  result.command_path = run('bash', ['-c', 'command -v rg']);
  result.real_path = fs.realpathSync(result.command_path);
  result.version_output = run('rg', ['--version']);
  result.version = result.version_output.split('\n')[0].split(' ')[1];
  result.binary_sha256 = crypto.createHash('sha256')
    .update(fs.readFileSync(result.real_path)).digest('hex');
  fixture = fs.mkdtempSync(path.join(os.tmpdir(), 'zg-rg-preflight-'));
  const file = path.join(fixture, 'fixture.txt');
  fs.writeFileSync(file, 'unrelated\nrg-preflight-needle-42\n');
  result.search_output = run('rg', ['--no-config', '--color', 'never', '-n',
    '--fixed-strings', '--', 'rg-preflight-needle-42', file]);
  result.status = 'ready';
} catch (error) {
  result.error = {name: error.name, command: error.command,
    exit_status: error.exit_status, stderr: error.stderr};
} finally {
  if (fixture) fs.rmSync(fixture, {recursive: true, force: true});
}
process.stdout.write(JSON.stringify(result) + '\n');
"""
    return "set -eo pipefail; . ~/.nvm/nvm.sh; node -e " + shlex.quote(script)


def validate_ripgrep_preflight(value: dict) -> None:
    _require_frozen_hashes()
    expected = {
        "schema_version": 1, "status": "ready", "version": RIPGREP_VERSION,
        "command_path": RIPGREP_PATH, "real_path": RIPGREP_PATH,
        "binary_sha256": RIPGREP_BINARY_SHA256,
        "search_output": RIPGREP_SEARCH_OUTPUT,
    }
    if any(value.get(key) != item for key, item in expected.items()):
        raise RuntimeError("ripgrep preflight did not match the frozen environment")
    if (not isinstance(value.get("cwd"), str) or not value["cwd"].startswith("/")
            or not value.get("path") or not value.get("user")
            or not isinstance(value.get("uid"), int)
            or not isinstance(value.get("gid"), int)):
        raise RuntimeError("ripgrep preflight omitted execution environment evidence")


def parse_ripgrep_preflight(stdout: str) -> dict:
    value = json.loads(stdout)
    if not isinstance(value, dict):
        raise RuntimeError("ripgrep preflight was not a JSON object")
    return value
