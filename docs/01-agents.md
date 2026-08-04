# Agent integrations

[Documentation](./README.md) · [Agents](./01-agents.md) ·
[CLI](./02-cli.md) · [MCP](./03-mcp.md) · [Pipeline](./04-pipeline.md) ·
[Architecture](./05-architecture.md) · [Server](./06-server.md) ·
[Embedding](./07-embedding.md) · [Roadmap](./08-roadmap.md)

`zg install` connects zvec-grep to supported agents through the local MCP
server. After that, the agent can use zg for conceptual discovery and exact
search without switching between unrelated search tools.

## Supported agents

| Agent | Target | Managed configuration |
| --- | --- | --- |
| Codex | `codex` | `~/.codex/config.toml` and `~/.codex/AGENTS.md` |
| Claude Code | `claude` | `~/.claude.json`, `~/.claude/settings.json`, and `~/.claude/CLAUDE.md` |
| OpenCode | `opencode` | `~/.config/opencode/opencode.json` and the adjacent `AGENTS.md` |
| Cursor | `cursor` | `~/.cursor/mcp.json` |

The standard environment overrides used by each agent are respected, including
`CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `OPENCODE_CONFIG`, and
`CURSOR_CONFIG_DIR`.

## Install an integration

Run the guided installer to select from detected agents:

```bash
zg install
```

For scripts or repeatable setup, select targets explicitly:

```bash
zg install --target codex --yes
zg install --target claude --target cursor --yes
zg install --target all --yes
```

The installer:

1. adds a managed `zvec_grep` MCP entry;
2. adds search guidance where the agent supports it;
3. adds local MCP tool approval for Codex and Claude Code;
4. starts the local zvec-grep server when possible.

The [Server guide](./06-server.md) explains when the daemon is useful and how its
lifecycle differs from Direct execution.

Managed text blocks use `ZVEC_GREP_START` and `ZVEC_GREP_END` markers. Existing
content outside those blocks is preserved. If an unmanaged `zvec_grep` entry
already exists, inspect it before using `--force` to replace it.

Restart the selected agent, or open a new session, after installation.

## How the agent searches

The default MCP surface gives the agent two complementary tools:

| Intent | Tool |
| --- | --- |
| The words, symbol, filename, path, or regex are known | `zvec_grep_rg` |
| The intent is known but the exact anchor is not | `zvec_grep_search` |

Both tools stay behind zg. The agent should refine the query or scope and try zg
again when a result is broad, instead of falling back to another local search
layer.

`zvec_grep_search` needs an existing index. `zvec_grep_rg` is local,
exhaustive, and works without one. See the [Pipeline guide](./04-pipeline.md) for
the distinction and the [MCP guide](./03-mcp.md) for tool inputs.

## Verify the setup

Check the server first:

```bash
zg server status --check-ready
```

Then start a new agent session and confirm that the `zvec_grep_search` and
`zvec_grep_rg` tools are available. If the MCP connection is unavailable, the
same search layer remains available from the shell:

```bash
zg query "where theme preferences are restored"
zg query --rg -F "loadTheme" src
```

## Permissions and remote data

The server listens on loopback and has no token by default. See
[Server authentication](./06-server.md#bearer-authentication) when a local Bearer
token is required.

MCP tool approval only authorizes calls to the local server. It does **not**
authorize sending query text or workspace content to a remote Embedding
provider. Remote Embedding asks separately on first use; see
[Embedding models](./07-embedding.md#remote-embedding-and-authorization).

## Remove an integration

Use the same target names to remove only zvec-grep-managed entries:

```bash
zg uninstall --target codex --yes
zg uninstall --target all --yes
```

Restart the agent or open a new session to apply the change. Uninstalling an
agent integration does not delete repository indexes or the npm package.
