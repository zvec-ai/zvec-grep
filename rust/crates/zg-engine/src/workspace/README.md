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
- **Compatibility.** Rebuild the index when its version does not match the
  version supported by the engine.
