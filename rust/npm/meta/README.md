# zvec-grep

This is the npm distribution wrapper for the native `zg` CLI. The package uses
an exact-version optional dependency to install only the native package matching
the current operating system, CPU architecture, and Linux libc.

Linux/Windows x64 install a complete CPU runtime first. The same executable
retains CUDA plugin-loading capability; no recompilation is needed to add GPU
support. Base packages pin optional component versions, core identities and
archive/file hashes in `plugins.json`, not npm dependencies.

`ZG_INSTALL_ACCELERATOR=auto` (default) downloads the existing complete CUDA
component set only when bounded NVIDIA detection succeeds. `cpu` skips both
detection and downloads; `cuda` forces installation even on GPU-free image
builders. Detection is only a download hint, not a CUDA compatibility test.
It runs `nvidia-smi` with a five-second timeout and checks for a successful,
nonempty result; it does not compare driver versions, probe CUDA/cuDNN libraries,
or choose different plugin subsets for GGUF and ORT.
Optional failures in auto mode leave CPU usable with a warning; base corruption
and forced CUDA installation failures are errors. Host NVIDIA drivers, CUDA
runtime libraries and cuDNN remain separate requirements.

Stop running zg/daemon processes before adding plugins. On Linux, for example:

```sh
ZG_INSTALL_ACCELERATOR=cpu npm install -g @zvec/zvec-grep
ZG_INSTALL_ACCELERATOR=cuda npm rebuild -g @zvec/zvec-grep --foreground-scripts
```

For project-local installations, omit `-g` and run in the project directory.
In PowerShell, set `$env:ZG_INSTALL_ACCELERATOR = 'cuda'` before `npm rebuild`,
then remove that environment variable when finished. There is no `zg-setup`
command. Restart any previously running daemon after successful installation.
CPU mode disables downloads; it does not uninstall already verified plugins.
Failed or interrupted CUDA additions leave the existing CPU payload intact.
After correcting the failure, rerun the same rebuild command; no separate
recovery command or download-resume feature is normally needed. Concurrent
installations are rejected. Stale-lock recovery is serialized and rechecks the
owner before removal; releasing a lock never deletes a replacement owner's lock.
If a process is killed during the brief lock-recovery operation itself, the
installer fails closed on `.native-install.lock.recovery`. Stop all installers,
inspect the lock and this guard directory in the installed meta package, and
remove the abandoned empty guard before retrying. Never remove it while an
installer is running. An invalid/partially written `.native-install.lock` also
requires inspection instead of automatic deletion.

`ZG_PLUGIN_DIR` selects an absolute directory containing the exact component
`.tgz` files (no registry/cache fallback if a file is missing).
`ZG_INSTALL_OFFLINE=1` or npm's `--offline` prevents network requests; without a
local directory only previously verified archives in `ZG_PLUGIN_CACHE` are used.
The default cache is below npm's cache, isolated by platform, version, core and
content identity. Runtime libraries are copied beside the binary, not loaded
from absolute cache paths. Online downloads use npm's registry/proxy/auth/CA
configuration and do not run component lifecycle scripts.

The installer verifies archives, streams Linux ORT fragments into a temporary
file, then checks the complete library hash before activation. It preserves
`bin/zg.exe`, `lib/` and `lib/cuda12/`; no compilation, driver/Toolkit installation
or model execution happens during installation. `native-install.json` records
the installed core and plugins; normal CLI execution never downloads plugins.
The `zg` command enters the native executable directly. Unix rpaths and the
Windows native delay-load hook locate the sibling `lib/` without a launcher.

Node.js 14.14 or newer is required only for installation. Online plugin downloads
need npm 7+; plugin extraction uses the system `tar` (included with the supported
Windows/macOS/Linux environments). CLI execution does not require Node or npm.

`--ignore-scripts` skips CPU bootstrap too and leaves a placeholder command.
Explicitly allow this package's lifecycle scripts in your npm policy, then use
`npm rebuild -g @zvec/zvec-grep --ignore-scripts=false` to initialize it. If the
platform dependency was omitted, reinstall without `--omit=optional` first.
