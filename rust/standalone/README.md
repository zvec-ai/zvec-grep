# Standalone installation from GitHub Releases

The standalone distribution uses the Rust native build and does not require Node.js
or npm on the user's machine. Its default download source is the official
[`zvec-ai/zvec-grep` GitHub Releases](https://github.com/zvec-ai/zvec-grep/releases).
The commands below become available when the updated release workflow publishes
its first standalone release; adding the installer files to the repository alone
does not publish an installation endpoint.

## Install and upgrade

macOS and Linux:

```sh
curl -fsSL https://github.com/zvec-ai/zvec-grep/releases/latest/download/install.sh | sh
```

Run the same command again to upgrade. To select a specific published version:

```sh
curl -fsSL https://github.com/zvec-ai/zvec-grep/releases/latest/download/install.sh | sh -s -- --version 0.3.3
```

Windows PowerShell:

```powershell
Invoke-WebRequest -UseBasicParsing https://github.com/zvec-ai/zvec-grep/releases/latest/download/install.ps1 -OutFile install.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File install.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File install.ps1 -Version 0.3.3
```

An explicit version resolves the manifest from its `v<VERSION>` Release, such as
`v0.3.3`. Without a version, the installer reads the Latest manifest once and
downloads all archives from that manifest's actual `tag`. An upgrade cannot mix
archives from two Latest releases. The matching GitHub Release must contain the
manifest and installation archives; creating the Git tag alone is not sufficient.
Checksums and sizes are verified before extraction, followed by native file-level
verification before activation. Downloads are HTTPS through GitHub and its asset
redirects; no GitHub token is required for public downloads.

Shell options and their PowerShell equivalents:

| Shell | PowerShell | Meaning |
| --- | --- | --- |
| `--activate VERSION` | `-ActivateVersion VERSION` | Select an already retained version without downloads; combine with an accelerator to select its variant |
| `--version VERSION` | `-Version VERSION` | Select a published version; omit for Latest |
| `--accelerator auto\|cpu\|cuda` | `-Accelerator auto\|cpu\|cuda` | Select optional NVIDIA components |
| `--install-dir PATH` | `-InstallDir PATH` | Choose an installation directory |
| `--home PATH` | `-DaemonHome PATH` | Select local daemon data; otherwise preserve the previous home |
| `--token-file PATH` | `-TokenFile PATH` | Authenticate shutdown/restart of a protected local daemon |
| Always user-managed | `-NoModifyPath` | Leave shell/User PATH unchanged |
| — | `-ModifyPath` | Re-enable Windows User PATH setup after a previous opt-out |
| `--skip-agent-upgrade` | `-SkipAgentUpgrade` | Leave existing integration launchers unchanged |
| `--uninstall` | `-Uninstall` | Remove standalone binaries while retaining user data |

`ZG_INSTALL_DIR`, `ZG_INSTALL_ACCELERATOR` and `ZG_INSTALL_SKIP_AGENT_UPGRADE=1`
provide corresponding environment defaults. The selected accelerator and Windows
PATH preference persist across upgrades. A custom directory must be passed again with
`--install-dir`/`-InstallDir` (or `ZG_INSTALL_DIR`); the installer never scans other
directories to discover installations. Initial installation into an unrelated,
nonempty directory is rejected.

Temporary downloads use `TMPDIR` on Unix (falling back to `/tmp`) and the system
temporary directory on Windows. `ZG_INSTALL_TMPDIR` overrides either location.

To test a fork, set `ZG_INSTALL_REPOSITORY=owner/repository`. Only GitHub Releases
are supported; the repository is recorded and reused on subsequent installs.
Download the installer from that same repository and use `--version`/`-Version`
for a preview release, because GitHub's Latest endpoint excludes prereleases.
For example, after publishing a `v0.3.3-rc.1` preview in `egolearner/zvec-grep`, test it with:

```sh
curl -fsSL https://github.com/egolearner/zvec-grep/releases/download/v0.3.3-rc.1/install.sh | ZG_INSTALL_REPOSITORY=egolearner/zvec-grep sh -s -- --version 0.3.3-rc.1
```

`ZG_INSTALL_REPOSITORY` never accepts a URL, host, or path outside a GitHub repository.

Unix needs a POSIX shell, curl or wget, tar with gzip support, standard text utilities, and either
sha256sum or shasum. Windows uses PowerShell and .NET ZIP/hash support. No compiler,
Python, jq or package manager runs during installation.

On Unix, the complete program and upgrade state live under `~/.zvec-grep/dist`.
The installer creates this absolute symlink to the stable executable:

```text
~/.local/bin/zg -> ~/.zvec-grep/dist/current/bin/zg
```

The shell installer never changes PATH or shell configuration files. If
`~/.local/bin` is missing from the current PATH, installation still succeeds and
prints commands to run and add to the user's chosen shell configuration. For
Bash/zsh and other POSIX-compatible shells:

```sh
case ":$PATH:" in
    *":$HOME/.local/bin:"*) ;;
    *) export PATH="$HOME/.local/bin:$PATH" ;;
esac
```

For fish, use `fish_add_path "$HOME/.local/bin"`. Verify command resolution with
`command -v zg` and `zg --version`. An existing npm command may take precedence
even when `~/.local/bin` is already on PATH; the installer reports that conflict.
The earlier shell `--no-modify-path` spelling remains an accepted no-op.

A custom `--install-dir` changes the link target while the command entry remains
`~/.local/bin/zg`. An unrelated file or symlink at that entry is not overwritten;
resolve it before retrying. The receipt records the owned launcher, so updates
reuse it and uninstall only removes it if it still points to this installation.

Windows keeps a small native launcher at `<InstallDir>\bin\zg.exe`. It reads the
`current` text pointer, resolves the selected executable, and forwards arguments,
standard streams and exit status. This does not require symlink privileges. The
installer updates User PATH unless
`-NoModifyPath` is selected. Open a new terminal afterwards. Its installer does
not create Unix-style command symlinks. npm-owned files are never overwritten.
Uninstall respects the saved PATH opt-out and an explicit `-NoModifyPath`.
It removes an entry only when the installer added it; pre-existing user entries
are preserved.

## Platforms and GPU behavior

Six base packages cover macOS ARM64/x64, Linux glibc ARM64/x64 and Windows MSVC
ARM64/x64. Linux requires glibc 2.28 or newer; Intel macOS requires macOS 14+.
There are no musl, macOS CUDA or ARM64 CUDA packages. macOS base packages retain
their existing Metal support; `cpu` here controls NVIDIA package installation,
not runtime `--device` selection.

Linux and Windows x64 additionally have complete CUDA archives. These use the
same extensible CPU core, plus the exact GGUF CUDA 13.1/12.4 and ORT CUDA 12
components. They contain no npm-specific ORT fragments.

- `cpu` downloads only the base archive and performs no NVIDIA detection.
- `auto` uses bounded `nvidia-smi` detection as a download hint. Failed optional
  downloads retain the base installation path and report the fallback.
- `cuda` explicitly requires the CUDA archive and fails on missing or corrupt
  assets. It can preinstall plugins on a machine without a GPU.

GPU detection does **not** prove driver, architecture, CUDA or cuDNN compatibility.
Drivers and CUDA runtime/cuBLAS/cuDNN remain host dependencies, just as for npm.
GGUF and ORT independently select their backends at runtime. Installation never
installs a driver or downloads models to test GPU execution. Use the existing
runtime diagnostics and `--device` controls to verify actual inference.

## Installation state and recovery

Default roots are `~/.zvec-grep/dist` on Unix and
`%LOCALAPPDATA%\zvec-grep\standalone` on Windows. Each root contains:

```text
.zg-standalone                  # Ownership marker
.install.lock                   # OS-held installation lock
0.3.1/darwin-arm64-cpu/bin/zg    # Immutable complete package
0.3.1/darwin-arm64-cpu/lib/
0.3.1/darwin-arm64-cpu/install-manifest.json
0.3.0/darwin-arm64-cpu/          # Retained version
current -> 0.3.1/darwin-arm64-cpu
install-receipt.json            # Active version, source, home and command ownership
install-state.tsv               # Bootstrap preferences and recovery executable location
.staging/                       # Candidate being verified
activation.json                 # Interrupted activation or uninstall transaction
```

Only `current` selects the active package. Unix replaces a temporary symlink using
same-directory rename; version directories and the command symlink stay in place.
Windows atomically replaces a text pointer named `current` and retains its native
launcher. The installer serializes changes with its installation lock and skips
replacing an unchanged selection. Executables resolve their actual version path at startup, so lazy loading
of plugins and other resources cannot cross versions during an upgrade.

All successfully staged versions are retained; there is no automatic pruning or
selection of the highest version. CPU and CUDA packages have separate directories.
Reinstalling the same version/target/variant reuses an identical verified package
and rejects a different payload at that immutable path. Uninstall removes all owned
version packages while preserving daemon data, configuration, models and indexes.

For an offline rollback, keep a local copy of the script and select an installed
full version explicitly:

```sh
sh install.sh --activate 0.3.0 --accelerator cpu
```

```powershell
.\install.ps1 -ActivateVersion 0.3.0 -Accelerator cpu
```

Activation uses no downloads or GPU probing. An `auto` accelerator preserves the
currently selected variant. The requested package must already exist and pass
manifest, checksum and executable-version checks. The daemon transaction and
agent migration rules are the same as for an online upgrade.

The binary invoked from staging performs the transaction on both OS families.
It copies and verifies the new bundle before taking the local daemon startup
claim, then gracefully stops only a process whose executable belongs to this
installation. A daemon from another installation does not block installation:
it stays running, without probing its authentication or requiring its token.
The installer reports that the runtime switch is pending and prints manual switching instructions.
Until switched, server/auto commands and MCP clients may keep using that daemon
while direct commands use the newly installed binary. Remote servers are not
controlled by the installer. A protected daemon owned by this installation needs
its existing token before it can be restarted.
Recovery records whether authentication is required, without storing token values.
After an interruption, supply the original token environment or `--token-file`;
missing credentials cannot restart a protected service anonymously. Saved token-file
paths are reused. Missing authentication metadata requires credentials rather than
assuming that anonymous startup is safe.

An active owned daemon restarts from the selected package with its previous listen
address, toolset and authentication settings. Failed activation restores the old
selection and service; an interrupted transaction is recovered when the installer
next runs. Recovery that needs to restart an owned daemon refuses to take over an
external daemon that appeared after the interruption; stop it explicitly before
retrying. Uninstall leaves external daemons running.

Uninstall records its committed state before deleting packages. Interrupted cleanup
resumes deletion instead of restoring a partially removed package. On Windows,
close direct CLI processes and MCP clients before uninstalling: locked EXE/DLL files
can delay deletion. Re-run the uninstall script to resume; if its cached recovery
executable has already been removed, the script downloads the same recorded release.
Ordinary upgrades do not replace or remove files used by older direct processes.

New integrations created through an installed standalone executable use its fixed
command entry, including after a rollback. Existing supported integration launchers can be migrated to the stable absolute
entry using `zg --install --upgrade-installed --launcher <ABSOLUTE_EXECUTABLE>`.
The existing configuration backup and comment-preservation behavior is retained.
With an external daemon, the installer only previews this migration (`--dry-run`):
configuration files and backups remain unchanged. Its default summary distinguishes
package installation from configuration migration, reports counts, and shows paths
and reasons only for errors or manual work. To inspect every entry, run:

```sh
"$HOME/.local/bin/zg" --install --upgrade-installed --launcher "$HOME/.local/bin/zg" --dry-run
```

Close all agents/MCP clients before applying the printed commands. When every
recognized integration uses stdio and retains the daemon's startup settings, the
default Unix flow is:

```sh
"$HOME/.local/bin/zg" --install --upgrade-installed --launcher "$HOME/.local/bin/zg" &&
"$HOME/.local/bin/zg" --server off --home "$HOME/.zvec-grep"
```

Only after those commands succeed, reopen agents; their stdio clients start the
new daemon. A failed migration never proceeds to shutdown. PowerShell uses guarded
`$LASTEXITCODE` checks so the same ordering works in Windows PowerShell 5.
HTTP integrations retain their URLs. HTTP clients, unsupported configurations or
startup settings not retained in stdio configurations receive an additional explicit
start command preserving the recorded home, listen address and toolset. Keep the
original token environment or `--token-file` for authenticated shutdown/startup and
when reopening agents. Token values are neither inspected nor printed by this advice.
With an owned daemon or no daemon, supported configurations migrate automatically;
the latter does not start a daemon. Existing arguments and environment settings are
preserved. Custom wrappers or unsupported configurations require manual edits.
Migration failures are reported without discarding a successfully installed
runtime. CI, root/elevated installs and the explicit skip flag do not migrate
personal integrations. Reconnect existing MCP/stdio clients after an upgrade.

Configuration, models and indexes survive uninstall. The Unix default places
programs in the managed `dist` subtree of `~/.zvec-grep`, leaving sibling data
untouched. Other overlaps between data and program directories are rejected.
Uninstall preserves `~/.local/bin`, shell configuration and user-maintained PATH
entries. If the command symlink was replaced with another program, it is retained.
Selecting an older release rolls back program files only: it does not undo an
index or configuration format migration. There is no native self-update command
in this first version; re-run the appropriate installer.

## Release workflow

`npm run release:standalone` assembles archives from the verified npm candidates,
reuses their file/core/plugin checks, and preserves licenses and notices. Node
and npm remain build-time tools. The output includes one `manifest.tsv`, both
installers and SHA256SUMS. Both installers read the same TSV without additional
dependencies. Two-column metadata rows contain `schema`, `version`, `tag`, the
source `commit` and `partial` (`true` or `false`). Five-column archive rows contain
target, accelerator, filename, SHA-256 and byte size. Fields must not contain tabs
or line breaks; LF and CRLF line endings are accepted. Duplicate metadata keys or
target/accelerator pairs, missing fields and malformed records are rejected before
archive downloads. Partial manifests support platform validation; stable publication
requires `partial=false` and all eight archives. The separate `install-manifest.json`
inside each archive still records package files, components and runtime requirements.

`rust-publish.yml` uploads per-target candidate artifacts, runs the standalone
install/update/failure/uninstall suite on all six CPU platforms with Node/npm
removed from installer PATH, and checks standalone CUDA inference in the existing
Linux/Windows GPU runtime environments. The normal npm gates remain in place.
Only an explicit official-repository `publish=true, dist_tag=latest` run from the
matching tag can publish standalone assets, after all required jobs succeed.

The publisher verifies the complete eight-archive inventory, uploads assets to a
draft GitHub Release, checks GitHub's asset sizes and SHA256 digests, and publishes
Latest last. Publication serializes the Latest comparison and update across tags.
Retries accept identical already-published assets and never replace them; an older
draft can be published without replacing a newer Latest. Partial platform runs are
validation candidates only. Fork previews can be published separately for testing;
the automated stable publisher remains restricted to the official repository.
No mirror, object storage or non-GitHub installation channel is supported.

The test harness uses a loopback HTTP server implementing the same GitHub URL
layout; its private test override is restricted to loopback and is never saved
as an installed download source.
