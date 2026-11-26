# zg-transport-mcp

This crate owns MCP schemas, input validation, presentation, setup and consent.
It translates tool inputs into typed engine requests or resident workspace
operations. HTTP and stdio expose the same tool behavior; the
[daemon](../zg-daemon/README.md) owns persistent jobs and workspace resources.

## Setup and consent

Missing-index setup selects a model and obtains remote consent when needed
before submitting a background build. Acceptance returns a job ID; later
requests report progress without repeatedly asking for setup. One-operation
consent covers that build only, not a later search.

Credentials must already be configured for MCP model selection. Existing
workspace consent is reused. Otherwise the host must support an explicit user
decision before remote indexing, search or refresh sends data. Unsupported
clients, cancellation, invalid responses and timeouts never grant consent.
A local-search choice disables vector retrieval and refresh.

One-time approval covers the data categories shown for each destination.
For example, a search approved to send query text needs separate approval
to send file contents discovered during refresh.

Continuation state is opaque, bounded, expiring and single-use. It binds the
caller, tool, unchanged arguments and disclosed destinations; consent is
rechecked before execution. A daemon restart invalidates pending continuations.
The shared [authorization contract](../zg-engine/README.md#remote-authorization)
and [workspace grants](../zg-engine/src/workspace/README.md#credentials-and-consent)
define the scope for both MCP and CLI.

## Input and presentation boundaries

Search filters narrow a request; they do not rewrite persisted scan policy.
Index requests own scan-policy updates. Reject unknown inputs and preserve the
distinction between omission and an explicit reset.

Preview changes presentation, not retrieval or ranking. Use the engine's
`content_range` and match coordinates for source numbering and highlighting;
do not infer source ranges from rendered text. Indexed full previews contain
the complete stored entity, not the whole file. Freshness and diagnostics are
part of the MCP text response, whereas CLI diagnostics use stderr.

Managed lexical commands are parsed as arguments and never executed by a shell.
Schema, presentation and accepted legacy differences are covered by
[compatibility fixtures](../../compat/mcp/README.md) and native regression tests.

## Progress and cancellation

Progress notifications describe indexing, refresh and model downloads. Their
monotonic sequence is not a percentage; completed index diagnostics require
waiting for completion rather than treating job submission as success.

Cancellation signals request-owned work and pending consent. It does not cancel
shared refreshes or background jobs owned by the daemon. Preserve cancellation
and resource ownership across transport and engine boundaries.
