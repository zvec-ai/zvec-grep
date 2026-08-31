# Agent integrations

[Documentation](./README.md) · [Agents](./01-agents.md) ·
[CLI](./02-cli.md) · [MCP](./03-mcp.md) · [Pipeline](./04-pipeline.md) ·
[Architecture](./05-architecture.md) · [Server](./06-server.md) ·
[Embedding](./07-embedding.md) · [Roadmap](./08-roadmap.md)

`zg install` connects zvec-grep to supported agents through the local MCP
server. After that, the agent can use indexed retrieval for workspace-grounded
semantic discovery while keeping exact lookup on the appropriate native or
managed-rg route.

## Supported agents

| Agent | Target | Managed configuration |
| --- | --- | --- |
| Codex | `codex` | `~/.codex/config.toml` and `~/.codex/AGENTS.md` |
| Claude Code | `claude` | `~/.claude.json`, `~/.claude/settings.json`, and `~/.claude/CLAUDE.md` |
| Qwen Code | `qwen` | `~/.qwen/settings.json` and `~/.qwen/QWEN.md` |
| Qoder CLI (`qoder` or `qodercli`) | `qoder` | `~/.qoder/settings.json` and `~/.qoder/AGENTS.md` |
| OpenCode | `opencode` | `~/.config/opencode/opencode.json` and the adjacent `AGENTS.md` |
| Cursor | `cursor` | `~/.cursor/mcp.json` |

The standard environment overrides used by each agent are respected, including
`CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `QWEN_HOME`, `QODER_CONFIG_DIR`,
`OPENCODE_CONFIG`, and `CURSOR_CONFIG_DIR`.

The current Qoder CLI package exposes both `qoder` and `qodercli` commands. The
installer accepts `qodercli` and `qoder-cli` as aliases for the canonical
`qoder` target, and automatic detection recognizes either executable.
`QODER_CONFIG_DIR` overrides the Qoder CLI configuration directory used by the
installer.

## Install an integration

Run the guided installer to select from detected agents:

```bash
zg install
```

For scripts or repeatable setup, select targets explicitly:

```bash
zg install --target codex --yes
zg install --target claude --target cursor --yes
zg install --target qwen --yes
zg install --target qoder --yes
zg install --target all --yes
```

The installer:

1. adds a managed `zvec_grep` MCP entry;
2. adds search guidance where the agent supports it;
3. adds local MCP tool approval for Codex and Claude Code, and managed server
   trust for Qwen Code and Qoder;
4. starts the local zvec-grep server when possible.

The [Server guide](./06-server.md) explains when the daemon is useful and how its
lifecycle differs from Direct execution.

Managed text blocks use `ZVEC_GREP_START` and `ZVEC_GREP_END` markers. Existing
content outside those blocks is preserved, as are unrelated settings and other
MCP servers. If an unmanaged `zvec_grep` entry already exists, inspect it before
using `--force` to replace it.

For Qoder, `--mcp-transport` selects either a stdio or HTTP entry under
`mcpServers`; the installer also manages the timeout and trust fields. Qoder CLI
search guidance is written to `${QODER_CONFIG_DIR:-~/.qoder}/AGENTS.md` without
replacing unrelated guidance.

Qoder receives the same MCP form-based Remote Embedding authorization request as
other clients. If it reports that no handler is registered for
`elicitation/create`, or returns a decline or cancellation without displaying
the form, the installed guidance supplies a compatibility interaction through
Qoder's `AskUserQuestion` tool. The agent must ask whether to allow Remote
Embedding for the workspace, use local FTS only, or cancel. It may run the
following persistent grant only after explicit workspace approval, then retry
the original MCP search once:

```bash
zg auth grant "/absolute/workspace" \
  --capability embedding \
  --scope workspace
```

The local-FTS choice retries the search without `query`, `queries`, or `vector`
routes and with `autoUpdate: false` and `freshness: "eventual"`. It neither
refreshes the remote-embedding index nor sends query text or workspace content
to a remote Embedding provider. In a headless session where Qoder cannot ask the
user, the agent stops without granting access. Provider credentials remain
separate from this data authorization.

Restart the selected agent, or open a new session, after installation.

## How the agent searches

The agent routes in two stages: first it decides whether the answer should be
grounded in the current indexed workspace, then it chooses exact or semantic
retrieval. Code versus non-code is not the boundary; a workspace may contain any
mix of code, documents, configuration, and data.

Workspace content is the intended evidence source when the user asks to inspect,
search, or ground the answer in local files, the workspace, or its index; prior
context established local material as the intended source; or the user asks
whether relevant local material exists. Negative, incidental, or comparative
workspace mentions do not establish relevance. Tool availability or topic
overlap alone does not establish workspace relevance.

The default MCP surface gives the agent one indexed search tool and leaves exact
lexical lookup to the agent's native tools:

| Intent | Tool |
| --- | --- |
| Workspace-grounded exact words, quotations, names, dates, keys, filenames, paths, or regexes are sufficient | Native grep or rg |
| Workspace-grounded wording or location is unknown, or the answer requires semantic, fuzzy, relationship, chronology, causality, comparison, or cross-file synthesis | `zvec_grep_search` |
| Exact anchors are known but the answer requires broader context or synthesis | `zvec_grep_search`, then native grep or rg |
| The answer is unrelated open-world knowledge, a current external fact, or web content that does not depend on local evidence | The appropriate external source, not zvec-grep |

`zvec_grep_search` needs an existing index. Managed rg remains available through
`zg query --rg` and through the optional `full` MCP toolset. See the
[Pipeline guide](./04-pipeline.md) for the distinction and the
[MCP guide](./03-mcp.md) for tool inputs.

When semantic discovery is selected because no sufficient exact anchor is
available and the user asks whether conceptually related material exists
locally, the agent makes at most one focused `zvec_grep_search` probe and stops
when the results are not relevant. Exact quotations, configuration keys,
filenames, regexes, and exhaustive occurrence requests stay on the exact route.

## Verify the setup

Check the server first:

```bash
zg server status --check-ready
```

Then start a new agent session and confirm that the client-specific search tool
is available. It is `zvec_grep_search` in Codex and Claude Code,
`mcp__zvec_grep__zvec_grep_search` in Qwen Code and Qoder, and
`zvec_grep_zvec_grep_search` in OpenCode. With the optional `full` MCP toolset,
Qoder exposes managed rg as `mcp__zvec_grep__zvec_grep_rg`. If the MCP
connection is unavailable, the same indexed search and optional managed-rg
route remain available from the shell:

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
zg uninstall --target qwen --yes
zg uninstall --target qoder --yes
zg uninstall --target all --yes
```

Restart the agent or open a new session to apply the change. Uninstalling an
agent integration does not delete repository indexes or the npm package.
