# Workspace

The workspace module manages workspace persistence and lifecycle.

## Storage layout

Each workspace stores its settings and index in the source directory's fixed
`.zvec-grep` child:

```text
source/
└── .zvec-grep/
    ├── manifest.json       saved settings and active index generation
    ├── index/              active index data
    ├── temp/               build and recovery data
    ├── locks/              workspace access coordination
    ├── credentials.key     optional key for saved encrypted API credentials
    └── authorization.json  optional remote-service consent records
```

Temporary and optional entries are created when needed. Downloaded models are
cached separately.

## Key conventions

- **Discovery.** Search and status walk upward to find the nearest workspace.
  A manifest or pending build record identifies a workspace.
- **Relocation.** Move or copy the source directory together with `.zvec-grep`.
  Moving `.zvec-grep` independently or rearranging its contents is unsupported.
- **Publication.** Incremental indexing updates the active index in place.
  First builds and rebuilds prepare an index in `temp/` and publish it after
  storage closes. Interrupted publication can be recovered; a failed rebuild
  before the manifest commits preserves the previous index.
- **Native recovery.** If an interrupted incremental write leaves zvec WAL
  records, the first storage reader closes its partial handles, takes the
  exclusive storage lock, and replays and flushes the existing collections.
  The caller retains its workspace read lock to prevent index publication.
  Storage returns to shared, read-only access before serving the request.
  Search, status, and automatic refresh checks use this same recovery path;
  unrelated storage failures remain errors.
- **Compatibility.** `indexVersion` determines index compatibility.
  The current format is v6, with PDF source mappings and a persisted vector engine.
  Incompatible indexes require `zg --index --rebuild`. A rebuild can reuse
  validated settings from a v2, v3 or v5 manifest. This preserves
  embedding, scan and graph settings; explicit options override saved settings.
  Unreadable settings must be supplied again.

## Read and write coordination

A cross-process writer-intent lock prevents new readers from overtaking a
waiting writer. Writers retire cached read handles before opening writable
storage. Lock waits are bounded and cancellable; cancelled or aborted waiters
must release their admission state. Native open/close is coordinated per
workspace so one cold workspace does not block unrelated cache hits.

Within one engine, queries with `Off` or `Background` refresh may borrow an
active incremental writer only when the effective model configuration matches.
Publication waits for borrowed queries. `Wait` and synchronous auto-update
queries never borrow partial writer state; a rebuild's replacement stays private
until publication.

Incomplete scans must not delete unseen files. Cancelled preparation preserves
existing content, and content-identical updates refresh metadata without
embedding again.

## Settings and relocation

Explicit index device and model-cache options become workspace defaults.
Omitted options preserve them. The model cache is shared by embedding and PDF
layout models and survives changing embedding models during a rebuild;
endpoints and credentials remain per model. Settings resolve from explicit
options, environment, workspace defaults, global configuration, then backend
defaults.
Query overrides remain temporary and must not be written back by automatic
refresh or retained by watchers. Moving a workspace may require overriding
machine-specific preferences without rebuilding its content.

Stored source paths and internal ignore-file references move with the workspace.
Missing ignore files retain their saved references; invalid rules and other
read errors remain errors. A move to a filesystem with filename collisions
requires rebuilding against the destination's actual files.

## Credentials and consent

Explicit indexing API keys are encrypted in the manifest. Inherited environment
or global credentials and query-only keys are not persisted.
`credentials.key` must travel with the index; copying the workspace also copies
access to its saved credentials.

`authorization.json` records remote-service consent separately from credentials.
A grant binds the canonical source root, username, model and endpoint. Changes
to those values need matching consent; ordinary manifest updates must not
rewrite the grant. These records are saved user decisions, not signed protection
against deliberate local edits. The [authorization API](../authorization.rs)
owns inspection, granting and revocation.
