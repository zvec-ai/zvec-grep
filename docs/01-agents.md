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
| Qoder CLI and IDE | `qoder` | `~/.qoder/settings.json`, `~/.qoder/AGENTS.md`, and the IDE user-level `~/.qoder/mcp.json` |
| OpenCode | `opencode` | the existing `~/.config/opencode/opencode.jsonc` or `opencode.json`, and the adjacent `AGENTS.md` |
| Cursor | `cursor` | `~/.cursor/mcp.json` |

The standard environment overrides used by each agent are respected, including
`CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `QWEN_HOME`, `QODER_CONFIG_DIR`,
`QODER_IDE_MCP_PATH`, `QODER_IDE_EXECUTABLE`, `OPENCODE_CONFIG`, and
`CURSOR_CONFIG_DIR`.

For OpenCode, `OPENCODE_CONFIG` selects the exact configuration file. Without
that override, the installer uses an existing global `opencode.jsonc` before
`opencode.json`, preserves JSONC comments and unrelated settings, and creates
`opencode.json` only when neither file exists. If both files exist, the selected
`opencode.jsonc` path is reported in the install output. On Linux and other
XDG-based environments, `XDG_CONFIG_HOME` replaces the default `~/.config`
root.

The current Qoder CLI package exposes both `qoder` and `qodercli` commands, but
the installer exposes only the canonical `qoder` target. One Qoder install
configures both the CLI and IDE, and automatic detection recognizes either CLI
executable or the IDE. `QODER_CONFIG_DIR` overrides the Qoder CLI configuration
directory; `QODER_IDE_MCP_PATH` independently overrides the full IDE `mcp.json` path, and
`QODER_IDE_EXECUTABLE` overrides the IDE executable used by automatic detection.

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
3. adds local MCP tool approval for Codex and Claude Code, managed server trust
   for Qwen Code and Qoder CLI, and exact search/rg allow rules for Qoder's
   CLI-backed runtime;
4. starts the local zvec-grep server when possible.

The [Server guide](./06-server.md) explains when the daemon is useful and how its
lifecycle differs from Direct execution.

Managed text blocks use `ZVEC_GREP_START` and `ZVEC_GREP_END` markers. Existing
content outside those blocks is preserved, as are unrelated settings and other
MCP servers. If an unmanaged `zvec_grep` entry already exists, inspect it before
using `--force` to replace it.

For Qoder CLI, `--mcp-transport` selects either a stdio or HTTP entry under
`mcpServers`; the installer also manages the timeout and trust fields. Search
guidance is written to `${QODER_CONFIG_DIR:-~/.qoder}/AGENTS.md` without
replacing unrelated guidance.

The same Qoder `settings.json`, used by Qoder CLI and CLI-backed Qoder clients,
receives exact `permissions.allow` rules for
`mcp__zvec_grep__zvec_grep_search` and `mcp__zvec_grep__zvec_grep_rg`, plus the
equivalent server-level `alwaysAllow` policy. This avoids repeated tool approval
prompts without allowing other MCP tools. Existing permission rules and JSONC
comments are preserved; uninstall removes only rules added by zg.

The same install writes the Qoder IDE MCP entry to its user-level
`~/.qoder/mcp.json`. Set `QODER_IDE_MCP_PATH` to override that complete file
path. The IDE entry uses
stdio when `--mcp-transport stdio` is selected. For HTTP it uses a `type: "sse"`
URL entry, from which Qoder IDE automatically detects the streamable HTTP MCP
endpoint. Qoder's platform `SharedClientCache` is runtime/cache state rather
than the user MCP configuration entry point, so the installer does not write
it. Qoder IDE has no supported global Rules file, so the installer does not
claim to create or manage IDE search guidance.

Both Qoder clients receive the same MCP form-based Remote Embedding
authorization request as other clients. If Qoder CLI reports that no handler is
registered for `elicitation/create`, or returns a decline or cancellation
without displaying the form, its installed `AGENTS.md` guidance supplies a
compatibility interaction through the exact `AskUserQuestion` tool. The CLI
agent must ask whether to allow Remote Embedding for the workspace, use local
FTS only, or cancel.

Qoder IDE uses the exact native tool name `ask_user_question`, but its official
integration does not provide a global Rules location where the installer can
persist the fallback. In stdio mode, the zvec-grep bridge turns a missing
`elicitation/create` handler into an actionable error that tells the top-level
IDE agent to present the same three choices with `ask_user_question`. This path
still needs an end-to-end smoke test in a real Qoder IDE session and should not
be treated as proof that every IDE build follows the instruction.

Only after explicit workspace approval may either client run the following
persistent grant and retry the original MCP search once:

```bash
zg auth grant "/absolute/workspace" \
  --capability embedding \
  --scope workspace
```

The local-FTS choice retries the search without `query`, `queries`, or `vector`
routes and with `autoUpdate: false` and `freshness: "eventual"`. It neither
refreshes the remote-embedding index nor sends query text or workspace content
to a remote Embedding provider. In a headless session where Qoder cannot ask the
user, the agent stops without granting access. Neither question tool should
collect a token, API key, or password. Provider credentials remain separate
from this data authorization.

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
`mcp__zvec_grep__zvec_grep_search` in Qwen Code and Qoder CLI, and
`zvec_grep_zvec_grep_search` in OpenCode. With the optional `full` MCP toolset,
Qoder CLI exposes managed rg as `mcp__zvec_grep__zvec_grep_rg`. For Qoder IDE,
confirm after restart that the `zvec_grep` server and its tools appear; the exact
host-qualified tool label remains part of the real-machine smoke test. If the
MCP connection is unavailable, the same indexed search and optional managed-rg
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
