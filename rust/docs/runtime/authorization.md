# Remote embedding authorization

CLI, daemon and MCP share workspace consent. API credentials and permission to
send data are separate. See [MCP](mcp.md) for transport and request lifetime.

## CLI consent

In an interactive terminal, `zg --index` prompts before sending data to an
unauthorized remote embedding destination. Choose `1. Allow once`,
`2. Allow for this workspace`, or `3. Cancel`. Direct and server modes show the
same prompt; server mode resolves the destination and saves workspace consent
using the server's configuration. Invalid input, EOF, or cancellation stops
before indexing. Existing consent and `--allow-remote` skip the prompt.
Non-interactive commands require existing consent or `--allow-remote`.

You can also authorize a workspace explicitly:

```sh
zg --auth grant /path/to/workspace --capability embedding --scope workspace --embedding qwen/text-embedding-v4
zg --auth status /path/to/workspace
zg --auth revoke /path/to/workspace
```

Granting consent does not send data, load a model, or build an index. Subsequent
remote indexing and semantic search may send workspace content and query text
to the selected endpoint and incur provider charges. API credentials remain
separate: use `ZVEC_GREP_API_KEY`, global model settings, or `--api-key` for
actual operations. An API key explicitly supplied when indexing is encrypted in
the manifest; inherited environment/global credentials and query-only keys are
not persisted.

The decryption key is stored in `.zvec-grep/credentials.key` and must travel with
the workspace. Copying the complete directory therefore copies access to its
saved credentials; this key does not grant permission to send data remotely.

## Workspace grants

Without `--embedding`, grant uses the existing index model, then
`ZVEC_GREP_EMBEDDING`, then the configured default model. `--endpoint` overrides
the stored index endpoint, per-model endpoint configuration, `ZVEC_GREP_ENDPOINT`,
and provider default. The grant at `.zvec-grep/authorization.json` binds the
canonical source root, username, model, and endpoint. A change to any of these
requires a matching grant. A copied grant remains valid on another machine when
the source path and username match. No authorization state or signing key is
stored in the user home directory, and no machine fingerprint is used.

Authorization records describe the user's saved consent; they are not signed or
protected against deliberate edits. Only explicit authorization updates the
recorded path and username; index updates and manifest writes do not. When the
username cannot be determined, use single-operation consent instead of a
persistent grant.

## Operation scope and MCP consent

Direct CLI, server CLI, and MCP read the same workspace authorization on each
operation. Revocation takes effect for new
operations without restarting the server. `--allow-remote` on `zg --index` or
`zg <query>` grants consent for that operation only, including its synchronous
refresh, and never authorizes later watcher jobs. MCP reuses existing workspace
grants and otherwise asks form-capable clients for explicit consent before index
or search sends data remotely. The form discloses the root, source roots, model,
endpoint and data categories. Choose `once` for this operation, `workspace` for a
persistent grant, or `cancel` to deny transmission. Search also offers
`fts_only`, which disables vector retrieval and refresh. Unsupported clients,
invalid responses, declined forms, cancellation and timeouts never grant consent.
Cancelling the originating request also sends a cancellation notification for its
pending consent form; clients control how that notification is presented.

## MCP continuation

For protocol `2026-07-28`, remote consent uses `input_required`. Echo the exact
`requestState` and unchanged tool arguments with
`inputResponses.remote_embedding_authorization = {"action":"accept","content":{"decision":"allow_once"}}`.
Other choices are `allow_workspace`, `use_local_search` (search only), and `cancel`.
The opaque state is kept in bounded daemon memory, expires after ten minutes,
binds the caller, tool, arguments and disclosed targets, and is consumed once.
Restarting the daemon invalidates pending prompts; request a new prompt. Older
clients retain reverse form elicitation. Consent is rechecked before execution.
