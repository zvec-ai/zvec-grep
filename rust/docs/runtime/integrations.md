# Agent integration and upgrades

Use `zg --install` and `zg --uninstall` to manage integrations. The
[public integration guide](https://zvec.org/en/docs/zvec-grep/agents/) covers
client setup; these notes record configuration ownership and supported upgrades.

## Upgrade existing launchers

`zg --install --upgrade-installed` migrates existing user-level MCP launchers
from 0.2.x without selecting agents or installing new integrations. It changes
`zg server --stdio` to `zg --server --stdio` and replaces recognized Node/JS
entrypoints with the running native executable. Legacy `zg` launchers that specify
an executable path also switch to the running native executable, so an old npm
prefix cannot keep invoking the Node.js version. Bare `zg` commands retain PATH
lookup, and launchers already using native syntax retain their executable paths.
Known config-directory overrides are respected; project files are not searched.
HTTP connections, toolsets, timeouts, environment variables, disabled state,
permissions and guidance remain unchanged. JSONC and TOML comments are preserved.
Only existing entries are changed, including when related clients use separate
config files.

Use `zg --install --upgrade-installed --dry-run` to preview. Each changed file
gets a unique adjacent `.zg-upgrade-*.bak` backup before atomic replacement.
Repeated runs make no additional changes. Invalid files and unsupported launchers
are reported while other independent files are processed; any unresolved entry
produces a nonzero exit status. Migration does not start a daemon or load models.
Reconnect agents afterwards. HTTP users can run `zg --server on` to replace an
old daemon using the existing protocol-upgrade path.

Published npm packages attempt this migration after a successful global
installation or rebuild for the current user. Local dependencies, temporary
execution, CI, elevated Unix installs and ambiguous user homes are skipped.
Set `ZG_INSTALL_SKIP_AGENT_UPGRADE=1` to opt out. Automatic migration has a
10-second timeout; failure preserves package installation and writes details to
`agent-upgrade.log` in the installed package. The manual command remains available
for skipped or failed migrations. Packaging and release verification opt out.

## OpenCode

`zg --install --target opencode` respects a nonempty `OPENCODE_CONFIG` override.
Otherwise it selects an existing `opencode.jsonc` before `opencode.json` under
`${XDG_CONFIG_HOME:-~/.config}/opencode`, creating `opencode.json` when neither
exists. Installation reports the selected path and explains when both files
exist. JSONC comments, trailing commas, unrelated settings and other MCP entries
are preserved. Uninstall removes managed entries from both global files, or only
from the explicit override, and removes managed guidance from the adjacent
`AGENTS.md`.

## Copilot and VS Code

`zg --install --target copilot` configures the user-level Copilot MCP server and
CLI instructions under `${COPILOT_HOME:-~/.copilot}`. `--target vscode` updates
every detected Stable and Insiders user profile, using `VSCODE_USER_DIR`,
`VSCODE_PORTABLE`, or `VSCODE_APPDATA` when set. It also registers the server in
Copilot's home because the shared instructions are read by both clients.
Uninstall preserves that shared server until both integrations are removed.

## Pi

`zg --install --target pi` configures Pi's built-in user-level MCP in
`${PI_CODING_AGENT_DIR:-~/.pi/agent}/mcp.json` and adds guidance to the active
global context file, including `AGENTS.override.md` when present. Both stdio
and HTTP use the shared daemon; new entries have direct exposure and a timeout
in seconds. Reinstall preserves Pi UI choices and existing HTTP authentication
unless an explicit token environment option replaces it. Uninstall uses a
stable ownership marker, so changing the daemon URL does not leave stale
managed entries. Use `/reload` or a new session. Native filesystem paths are
required for the directory override; file URIs and Windows shell drive paths
are rejected. Pi's built-in MCP currently needs user-managed `zg --auth grant`
authorization for Remote Embedding; see the
[integration guide](https://zvec.org/en/docs/zvec-grep/agents/).
