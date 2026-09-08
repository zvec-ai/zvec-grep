# Server and execution modes

[Documentation](./README.md) · [Agents](./01-agents.md) ·
[CLI](./02-cli.md) · [MCP](./03-mcp.md) · [Pipeline](./04-pipeline.md) ·
[Architecture](./05-architecture.md) · [Server](./06-server.md) ·
[Embedding](./07-embedding.md) · [Roadmap](./08-roadmap.md)

The zvec-grep Server is a local daemon shared by agents and terminal commands.
It keeps the MCP endpoint available, coordinates active Workspace runtimes,
supports background index refresh, and can reuse loaded Embedding models across
requests.

You do not need the Server for every use of zg. Exact managed ripgrep and
indexed operations can also run directly in the current process.

## Choose an execution mode

Indexed CLI commands accept `--mode auto|server|direct`.

| Mode | Use it when | Behavior |
| --- | --- | --- |
| `auto` | Almost all terminal use | Searches start or reuse a local Server when indexed retrieval is needed; other operations use a ready Server or run Direct |
| `server` | A script requires the daemon, shared state, or background refresh | Require the Server and fail if it is unavailable |
| `direct` | One-off use, CI, foreground debugging, or no daemon is desired | Run entirely in the current process |

`auto` is the default. Ordinary search first checks live files, so a verified
code-shaped symbol lookup needs neither an index nor a daemon. Indexed searches
start or reuse a missing local Server, with a three-second startup wait budget.
If startup fails, local execution remains available. Non-local or nonstandard
MCP URLs are not implicitly launched, and a different running endpoint is never
restarted or replaced. `--mode direct` disables implicit daemon startup.

Implicit startup creates a private Bearer token in the daemon home (unless an
explicit token is configured). Concurrent starters reuse that token and daemon.
No source code is sent to a remote embedding provider by implicit indexing.
When the index is not usable and a default `auto` query has no literal matches,
a lazy, bounded keyword scan runs before the readiness wait. Useful hits return
as `matchedBy=keyword` with an incomplete-coverage warning while local indexing
continues; an existing job is reused. These early results may omit semantic
neighbors; ready-index retrieval and explicit `--hybrid`, `--vector`, and
`--refresh wait` are unchanged.

Only when neither literal nor keyword matches exist does ordinary default search
give initial preparation up to eight seconds after startup. Status polling and
scheduling share that deadline; polling does not restart the job, and longer
builds remain background work. This is not a total CLI latency guarantee. An
initialized collection alone is not proof that the first build is ready.
`--refresh wait` requests foreground waiting beyond this preparation budget.
The separate [query-model preparation budget](./02-cli.md#zg) applies to ordinary
daemon queries against an existing local-model index; it is not a deadline for
the whole search or for the independently owned background indexing job.
The shared model pool evicts idle models after 15 minutes. Workspace watchers and
their lightweight runtimes are released after four hours without a client
request or a relevant file-system change by default; active work is not evicted.
See [Server lifecycle](#server-lifecycle) for the configurable watcher timeout.
The daemon remains available until `zg --server off`.

Direct MCP search calls do not perform the CLI's implicit index preparation.
An ordinary single-query eventual search can return current-file literal and
bounded keyword matches when no local index is usable, without loading a model
or creating an index job. It warns that semantic coverage is incomplete;
explicit routes, freshness waits, and existing remote-index authorization keep
their prior behavior. See the [MCP search contract](./03-mcp.md#zvec_grep_search).

Use Server mode when:

- an Agent connects through MCP;
- indexed searches happen repeatedly across sessions;
- background refresh should keep an active Workspace current;
- multiple requests can benefit from a shared loaded model;
- operational tooling needs an explicit ready/not-ready contract.

Use Direct mode when:

- a command is an isolated one-off operation;
- a CI job should not leave a daemon running;
- you want foreground failures and resource lifetime tied to one process.

Managed `zg --rg` does not need an index or loaded Embedding model. It
runs locally regardless of whether a Server is available.

## Agent setup

`zg --install` configures the selected Agent and starts the Server when possible:

```bash
zg --install
```

Most Agent users therefore never need to run `zg --server on` manually. Restart
the Agent or open a new session after installation so it discovers the MCP
endpoint.

Set `ZVEC_GREP_INSTALL_SKIP_SERVER=1` only when another process manager will
start the Server separately.

See [Agent integrations](./01-agents.md) for managed configuration and
[MCP](./03-mcp.md) for the exposed tools.

## Server lifecycle

Start the background daemon:

```bash
zg --server on
```

Inspect process readiness, endpoint, PID, and MCP toolset:

```bash
zg --server status
zg --server status --check-ready
```

`--check-ready` preserves normal output and exits non-zero unless the Server is
ready, making it suitable for scripts and health checks.

Stop the daemon gracefully:

```bash
zg --server off
```

Run it in the foreground for logs or process supervision:

```bash
zg --server run
```

Only one Server instance can own a given zvec-grep home. If a running Server
uses the wrong MCP toolset, stop it before restarting with the new profile:

```bash
zg --server off
zg --server on --mcp-toolset full
```

The Server releases a Workspace watcher and its lightweight runtime after four
hours without a client request or a relevant file-system change. Periodic
reconciliation does not extend this idle deadline. To select another timeout,
set the number of seconds before starting or restarting the Server:

```bash
export ZVEC_GREP_WATCHER_IDLE_TIMEOUT_SECONDS=7200
zg --server off
zg --server on
```

Set the value to `0` to keep activated watchers until the Server stops.

## Configure the mode

Choose a mode for one command:

```bash
zg --mode direct "root-local index discovery"
zg --status --mode server --check-ready
```

Set an environment default:

```bash
export ZVEC_GREP_MODE=auto
```

Or set `client.mode` in `~/.zvec-grep/config.json`:

```json
{
  "version": 1,
  "client": {
    "mode": "auto"
  }
}
```

An explicit `--mode` wins over the environment and global configuration.

## Refresh behavior

The execution mode changes the default indexed-search refresh policy:

| Policy | Server | Direct |
| --- | --- | --- |
| Default | `background` | `off` |
| `--refresh background` | Return current results and schedule an update | Warn and behave as `off` |
| `--refresh wait` | Wait for a fresh index | Update and wait in the current process |
| `--refresh off` | Search without updating | Search without updating |

Server searches return `freshness: possibly_stale` only with evidence of index
drift. The first refresh after activation may use this conservative status; an
hourly reconciliation remains `fresh` until its probe finds a mismatch. Use
`--refresh wait` only when the latest file state is required.

Returned-source hash mismatches, missing files, and unverifiable indexed source
also count as drift, even when size and modification time did not change.
Background refresh coalesces those exact paths with watcher updates. Freshness
remains unconfirmed until the repaired paths are checked against the actual
committed content; unrelated job success does not clear them. `off` records but
does not repair, while `wait` repairs and rechecks or reports a bounded
reconciliation error. A failed/cancelled repair is not restarted by every query
for the same observed version; a subsequent edit or explicit `zg --index` can
retry. Automatic remote source repair requires a Workspace Remote Embedding
grant, never just a one-time query permit.

Watcher-reported path updates skip workspace-wide status scans. Because
file-system watchers can silently miss events, the Server schedules an hourly
full reconciliation probe; the next search uses it to scan the Workspace and
repair index drift.

Large bursts of exact watcher events are compacted into directory-scoped
updates. A full reconciliation is reserved for watcher errors, missing event
paths, resume drift, and other cases where events may have been lost.

## Endpoint and toolset

The default endpoint is:

```text
http://127.0.0.1:7999/mcp
```

The Server only accepts loopback listen addresses. Change the loopback address
or port with:

```bash
zg --server on --listen 127.0.0.1:8999
```

Set `ZVEC_GREP_SERVER_URL` when a client should use a non-default configured
endpoint.

The default `agent` MCP toolset exposes only `zvec_grep_search`. Use
`--mcp-toolset full` or `ZVEC_GREP_MCP_TOOLSET=full` to expose optional managed
rg together with the index and status tools. See [MCP](./03-mcp.md) for the tool
contract.

## Bearer authentication

Implicit search startup enables authentication with an automatically generated
private token. Manual startup without a token remains unauthenticated. To
configure a token explicitly, provide at least 32 characters through the environment or a
file:

```bash
export ZVEC_GREP_SERVER_TOKEN="replace-with-a-long-random-token"
zg --server on
```

```bash
zg --server on --token-file /secure/path/zvec-grep.token
```

Clients can use `ZVEC_GREP_SERVER_TOKEN` or `ZVEC_GREP_SERVER_TOKEN_FILE`.
Supported Agent integrations can reference an environment-backed token:

```bash
zg --install \
  --target codex \
  --mcp-token-env ZVEC_GREP_SERVER_TOKEN \
  --yes
```

The MCP Bearer token protects the local Server. It does not configure an
Embedding provider or authorize remote data transfer.

## Logs and state

The default daemon directory is `~/.zvec-grep/daemon/`. Its structured JSON
Lines log is written to:

```text
~/.zvec-grep/daemon/logs/server.log
```

Credential, authorization, token, API-key, and query fields are filtered from
daemon log records. Repository identities are logged opaquely rather than as
raw paths where identity is sufficient.

Use `ZVEC_GREP_HOME` to relocate Server state. Check `zg --server status` before
reading logs; routine searches do not need a status preflight.
