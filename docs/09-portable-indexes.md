# Move and reuse an index

[Documentation](./README.md) · [CLI](./02-cli.md) · [MCP](./03-mcp.md)

A change in workspace location should not require another embedding run for
unchanged documents. Use a portable index to keep stored vectors when you move
an indexed workspace. These instructions apply to the Node.js implementation.

## Choose the operation

| Need                                                    | Operation                                          |
| ------------------------------------------------------- | -------------------------------------------------- |
| Move a complete workspace on a compatible host          | Relocation of the files and `.zvec-grep/` together |
| Convert an old absolute-path index (manifest version 1) | Migration to manifest version 2                    |
| Transfer across hosts or native database environments   | Logical export and import                          |

Keep a restorable copy before you change a workspace. Stop indexing and close
services that use it before you copy native index files. Do not copy a live
database. A successful search alone does not prove that document vectors were
reused; use the verification checks below.

## Destination and model requirements

- The workspace must be self-contained. All configured roots must resolve
  inside it. Copy the source documents with the same relative paths.
- A migration or import destination must not already contain a workspace index.
  An export artifact path must be new. Keep artifacts outside indexed roots.
  Do not remove an existing index to make a failed command succeed.
- Migration and export read private copies of native storage. Allow temporary
  disk space for the source collections as well as space for the destination.
  The copies are removed before publication. Native reader metadata changes
  cannot affect the original index.
- Use a version that supports the artifact format and portable manifest.
  Logical import creates native storage on the receiving host. It does not
  require compatible native database files from the sending host.
- Credentials and host bindings do not transfer. Configure credentials on the
  receiving host. Configure the same embedding model for vector queries and
  incremental indexing; install its model files there if it is a local model.
  Device selection is local to the host.
- Remote query or document embeddings need the normal authorization on the
  receiving host. Transfer does not grant that authorization.
- An artifact contains indexed text, paths and vectors. Protect it as you
  protect the source documents. Removal of credentials is not encryption.

## Relocation

Move the complete workspace and its `.zvec-grep/` directory together. Preserve
the paths inside the workspace. The portable manifest stores workspace-relative
paths; the receiving process creates its own host binding. It retains the index
identity. Native file copies still require a compatible storage environment.
Use logical transfer when that compatibility is not established.

Converting this complete directory into a Git submodule does not itself change
its internal paths. Git does not transfer an ignored `.zvec-grep/` directory.
Transfer that index separately. These commands do not split a larger index or
extract one indexed subtree into an independent index.

## Migration

This example converts a legacy index into a separate workspace that already
contains the corresponding documents:

```bash
zg --migrate-index /old/project/.zvec-grep /new/project
```

The source index is not modified. The destination is built in a staging
directory, verified, and then published. The workspace index identity and
vectors are preserved; path-derived file and fragment identifiers change to
the portable form. Review the returned missing-file list. Do not claim a
complete workspace when source documents are absent.

Verification reports `vectorsExact` and `vectorsPreserved` separately.
For cosine storage, a native write/read can change a component by float32
rounding. `vectorsPreserved` permits at most two float32 representable steps
per component. Dot and Euclidean storage still require exact equality.
Non-finite values, different dimensions and larger changes fail verification.
This rule does not permit a different model or recompute any embedding.

### Replace a legacy index at the same workspace root

Migration does not overwrite an occupied destination. Do not move the working
legacy index aside before conversion. Use this sequence:

1. Stop writers, watchers and services for the workspace. Keep them stopped
   through replacement. Make and verify a restorable backup.
2. Create an empty temporary workspace outside the source workspace. Run
   `zg --migrate-index /project/.zvec-grep /temporary/workspace`.
   The source remains in place while conversion runs. If the temporary workspace
   has no documents, the missing-file list is expected; it is not evidence of
   lost index records. Check the exit status and all verification fields.
3. Only after successful verification, rename `/project/.zvec-grep` to a new
   backup name on the same filesystem. Move the verified
   `/temporary/workspace/.zvec-grep` to `/project/.zvec-grep`.
   If the second move fails, restore the backup before restarting clients.
   Do not overwrite or delete an existing backup.
4. Reopen the index at `/project`. Check identity, expected query hits and
   the missing-file list there. Keep the legacy backup until acceptance.
   The first explicit indexing run reconciles files by content hash.

### Memory and failed operations

Migration and logical transfer stream text and vectors. Memory still grows
with native storage and file/fragment identity maps, but the application does
not retain all source and destination vectors as JavaScript arrays. Increasing
the Node heap alone does not correct a failed vector verification.

CLI and MCP operations run in a separate process. A fatal native error can stop
that operation without stopping the caller. Normal errors and cancellation
run checked cleanup. A fatal exit can leave a destination marked `INCOMPLETE`.
The error reports that state as unresolved, not as a clean abort.

The CLI reports `vectors exact` only when every compared component is exact.
Otherwise it reports `vectors preserved` when the cosine tolerance passes.
The result states the number compared and whether that check used a sample.
A sampled result is not proof that every vector was compared. Differences
outside the permitted tolerance still fail the operation.
The CLI selects a sample of at most 256 vectors. MCP compares all vectors.

The CLI prints `Temporary transfer data: <path>` before the operation starts.
On Linux and macOS, `SIGINT` and `SIGTERM` request checked cancellation. A
forced process exit, such as `SIGKILL`, cannot run normal cleanup. After both
recorded processes have exited, remove only that private copy with:

```bash
zg --cleanup-transfer /tmp/zg-portability-process-XXXXXX
```

Use the exact printed path; the temporary directory can be elsewhere. This
command checks the host, user, parent and child process IDs, directory identity
and ownership record. It accepts aliases in parent directories, such as macOS
`/var`. It refuses active processes, copied records and a symlink at the scratch
entry itself.
It does not remove source or destination locks, index data or `INCOMPLETE`
markers. Older `zg-transfer-source-*` copies have no ownership record and must
be inspected manually. Never remove directories by name pattern alone.

For an abandoned destination, stop all users of that workspace and confirm
that the recorded owner process is dead on the recorded host. Keep the source
index and its backup. Preserve the failed destination under a new quarantine
name before retrying with a new, empty destination. Do not remove only the
`INCOMPLETE` marker or treat partial files as a completed index. Dead reader
locks are reclaimed by the existing lock protocol; active locks are not removed.

## Logical export and import

On the sending host:

```bash
zg --export-index /source/project/.zvec-grep /transfer/project-index
```

The source is read under a lock that excludes writers. The artifact contains
versioned metadata, file records and fragments with vectors. No model runs
during export. Copy the complete artifact and the source documents to the
receiving host. This copy is a separate operation, outside the CLI and MCP.
Verify file inventory and checksums after the copy, before import.

On the receiving host, with the documents at their original relative paths:

```bash
zg --import-index /received/project-index /destination/project
```

Import validates the artifact and builds native storage in a staging directory.
It verifies counts, identities, ownership, inventories, groups and vectors
before publication. It does not compute embeddings. Keep the original artifact
and source index until verification is complete. A lock conflict or occupied
destination is an error; resolve the owner of that resource before retrying.

Migration, export and import are also available through the full MCP toolset:
`zvec_grep_index_migrate`, `zvec_grep_index_export` and `zvec_grep_index_import`.

Migration and import return `missingFilesCount`, `missingFilesTruncated` and
a `missingFiles` sample. By default the sample has at most 20 paths and 4096
path characters. Text and structured results use the same sample. When the
complete list is required, set `includeAllMissingFiles: true` in the initial
operation request. That explicit response can be large. The engine and CLI
keep the complete count; truncation does not change stored index data.
Each requires `confirm: true` after an explicit user request. Existing MCP
search and indexing tools can read and update the result. A tool path is a path visible to the server,
not necessarily to the agent's computer. An agent must have an explicit user
request before it creates or changes a persistent index.

## Verify reuse and updates

1. Record the source index identity from `.zvec-grep/manifest.json` and the
   source revision. After transfer, compare the destination manifest identity.
2. Check command exits and conversion results, including missing files and
   verification fields. For a copied artifact, compare the full inventory and
   checksums. A transport success message is insufficient.
3. Search for expected content and verify its file path under the destination.
   Query embeddings can still occur. They are separate from document embeddings.
4. The migrated or imported index starts unverified. Its first indexing run
   reconciles content by hash. Unchanged documents reuse vectors; changed
   documents can require new embeddings. Record document-embedding calls when
   testing a zero-document-embedding claim.
   Host verification uses the native resolved workspace path and filesystem
   identity. Windows short-path aliases refer to the same binding. An older
   binding with a different path spelling can require one reconciliation.
5. In a disposable copy, change one document and remove another. Run indexing
   explicitly. Check the changed content, removal of deleted files from stored
   entries, and preservation of unrelated documents. Retain the output and exits.

Status reports an unverified index as needing an update. A default MCP search
schedules reconciliation when automatic refresh is enabled. Set
`freshness: "wait_for_fresh"` to wait for that work. If automatic refresh is
disabled, search keeps the unverified state and reports stale results.

See `zg --help migrate`, `zg --help export` and `zg --help import` for the
installed command contract. The [design contract](./design/portable-workspace-index.md)
states the identity rules and the limits of the recorded evidence.
