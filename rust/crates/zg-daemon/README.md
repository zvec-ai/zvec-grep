# zg-daemon

The resident host owns one shared `ZvecGrep`, workspace coordination, watchers
and indexing jobs. HTTP and stdio use the same MCP tool implementation.
[Architecture](../../ARCHITECTURE.md) defines the crate boundaries;
[zg-transport-mcp](../zg-transport-mcp/README.md) owns tool schemas and presentation.

## Resident ownership

Workspaces share the engine's model runtimes and generation-specific read
sessions. Foreground work and active jobs prevent workspace retirement.
Background changes and completed jobs must not keep an otherwise idle workspace
alive indefinitely. Retirement releases resident state without deleting the
persisted index; a later request can activate it again.

Refresh coordination preserves the caller's policy. Concurrent waiting requests
can share one refresh. A clean background request does not need another job.
Watcher installation, missed events, recovery and failed indexing invalidate
reconciliation evidence; an older successful job must not clear newer changes.
Freshness requires observed file-change evidence independently of job activity.

## Request and job lifetime

Request cancellation stops waiting and cooperatively cancels work owned only by
that request. Shared jobs, watcher refreshes and already-submitted background
jobs remain daemon-owned. Native work may finish its current non-interruptible
step. Transport disconnects must not discard resources still used by that work.

## Direct and Server modes

Both modes use the engine's typed operations and must preserve matching
semantics. Intentional differences are recorded and tested in
[mode-differences.toml](../../compat/mode-differences.toml):

- Direct resources live for one command; Server resources are reused.
- Default indexed refresh is off in Direct mode and background in Server mode.
- Server freshness can include watcher evidence unavailable to Direct execution.
- CLI path lookup and managed lexical search stay local in every mode.

## Protocol and process lifecycle

Check the instance protocol version before sending requests. Capability flags
gate optional request fields; unsupported capabilities require an upgrade or
restart instead of silently changing the request's meaning. An incompatible
live daemon must not cause `auto` mode to silently execute directly.

Content-kind routing uses daemon protocol v3. Replace an older resident
daemon with `zg --server on` using the same home and token settings.

Supported daemon replacement serializes shutdown and startup, validates process
identity and checks the destination before stopping the old service.
Unreachable endpoints, rejected shutdowns and exit timeouts preserve the process.
The stdio bridge closes when its resident process exits or is replaced.
