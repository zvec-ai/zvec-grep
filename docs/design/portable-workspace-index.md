# Portable workspace index — identity and path contract

Design status: **implemented and demonstrated for the Node.js fork**.
The specification baseline was accepted on 2026-09-15. On 2026-10-04,
source `72422a9` passed owner-run tests on macOS 26.6.2 arm64: indexing,
search, reopen, relocation, separate-process transfer, changes and deletions.
Its test index also opened on a separate Linux x64 host with the same index
identity, expected content and zero document embeddings. A repeated native
Mac archive transfer passed without changes to the received payload.

These observations cover the recorded native-index transfer and test harness.
They do not establish arbitrary native database compatibility, older macOS
support, Gatekeeper/notarization, or new MCP migration/export/import tools.
Logical export/import has separate engine integration coverage. New MCP tools
need their own interface and host evidence. The Rust implementation is outside
this specification's demonstrated scope. Use the practical
[Move and reuse an index guide](../09-portable-indexes.md) for instructions.

Objective: an index built in one location (for example macOS with Metal) can be
transferred with its workspace to another location or host (for example Linux
with CPU) and then searched and incrementally updated **without re-embedding
unchanged documents**. Query embedding remains a normal per-search operation.

## 1. Scope and definitions

- **Workspace**: the directory that contains `.zvec-grep/`. Index discovery is
  the existing upward walk; the physical layout does not change.
- **Self-contained workspace**: every configured scan root resolves inside the
  workspace root after symlink resolution. Version 1 supports only
  self-contained workspaces; a scan root outside the workspace is an explicit
  error in portable mode. External roots need a later mapping design.
- **CRP (canonical workspace-relative path)**: a file's location inside the
  workspace, defined in §2.

## 2. Canonical workspace-relative path

1. The CRP is the file's path relative to the workspace root: segments joined
   with `/`, no leading or trailing slash, no `.` or `..` segments, Unicode
   NFC-normalized, letter case preserved from the actual directory entry.
2. A CRP is derived from actual directory entries (`readdir` results), never
   from user-supplied strings alone.
3. **Resolution** (CRP → current absolute path): per-segment lookup against the
   current filesystem. For each segment, read the containing directory and
   select the entry whose NFC form equals the segment. Resolution is
   tri-state: **ok**, **missing** (no entry), or **forbidden** (an entry
   exists but escapes the workspace through a symlink, including any
   intermediate component). A forbidden result is an explicit containment
   error; no caller may reconstruct a readable pathname for it. A display
   fallback is permitted for genuinely missing files only.
4. **Collision rejection**: during scanning, two entries in one directory whose
   NFC forms are equal, or whose case-folded NFC forms are equal, are an
   explicit scan error naming both. This protects case-insensitive and
   normalizing destination filesystems.
5. **Containment**: a stored CRP that would resolve outside the workspace root
   (through `..` or a symlink) is rejected. Persisted roots are re-validated
   for real containment at every scan and at open; a pre-set canonical path
   never bypasses the check. A scanned entry whose symlink target escapes the
   workspace root is excluded with an explicit diagnostic. Backslashes are not
   valid in stored portable data, so it cannot change meaning across
   platforms' separators.
6. `/` is the stored separator on every platform, including Windows.

## 3. Persistent identities

1. The workspace index UUID (`manifest.id`) is unchanged and is preserved by
   copying and by conversion.
2. `fileId = sha256hex(workspaceIndexId + "\0" + CRP)`. The separator
   convention matches the current scheme; the location input replaces the
   absolute path.
3. `fragmentId = sha256hex(fileId + "\0" + index)` — unchanged derivation;
   fragment IDs stay stable because file IDs stay stable.
4. Group references keep their current semantics (a group's value is its major
   fragment's ID) and are stable for the same reason.
5. **Renames**: version 1 treats a rename as delete + add under the existing
   diff semantics. That is correct (no stale or orphaned records) but not
   optimized: renamed files are re-embedded. Vector transfer or ID-preserving
   rename maps are a separate, later decision.

## 4. Persistent data vs host bindings

The manifest (format version 2) stores only portable data:

- `id`, `name`, `manifestVersion: 2`
- `rootPaths`: scan roots as CRPs with their existing selection options;
  ignore-file references are also stored as CRPs and resolved against the
  current workspace with containment checks
- `indexPolicy`, `embedding` (provider, model, dimension, metric — the
  endpoint stays part of the embedding identity per current rebuild rules),
  `indexVersion`, `createdTime`, `updatedTime`

The manifest does **not** store: its own absolute location, absolute root
paths, `device`, `apiKey`, or any verification claim.

- **Storage location**: always the discovered `<workspace>/.zvec-grep`. A
  persisted location is unnecessary and is removed; the stale-binding failure
  mode disappears structurally.
- **Device**: runtime-only, resolved per host with the existing `auto`
  default. Never persisted, never inherited across hosts.
- **Credentials**: `apiKey` is never persisted in the manifest. Remote
  providers resolve credentials per session from explicit options, environment,
  or global configuration, as they already can.
- **Verification**: content-verification state lives in a host-local binding
  store in the global home (never inside the workspace), keyed by index UUID
  and physical binding. Transferred indexes therefore carry no verification
  claim at all (§5).
- **Locks and runtime caches**: remain keyed by the canonical physical root
  (existing daemon behavior). Two copies of one index on a single host are two
  distinct physical roots and never share a writer or storage handle.

## 5. Rebind reconciliation

1. Verification state is a **host-local binding record** (global home, keyed
   by index UUID, bounded in size): the physical workspace binding (realpath
   with actual filesystem spelling, plus the storage instance's device/inode)
   and the time it was content-verified. It is excluded from transfer
   artifacts by construction.
2. An index is **unverified** whenever the current binding cannot be
   established against the record — missing, mismatched, or unreadable. Any
   doubt means unverified; no token inside the transferred data is trusted.
   Status and freshness logic report unverified indexes as needing refresh.
3. The next indexing run on an unverified index performs a **reconciliation
   pass** (always a complete pass, never changed-paths-only): every scanned
   file that matches a stored record by file ID is content-hashed regardless
   of size/mtime agreement.
   - Identical content: the record and its vectors are reused. Stored size and
     mtime metadata are refreshed. No embedding calls occur.
   - Changed content: normal incremental handling.
   - Missing files: normal deletion handling.
4. The binding record is published **only after** a successful reconciliation,
   and per-UUID updates are **serialized** (a per-index lock guards the
   read-modify-write, always taken inside workspace locks, never around them),
   so a concurrent or stale update cannot resurrect an invalidated record.
   Every supported restore/replacement workflow (import, migrate,
   rebuild/reset, drop) **explicitly invalidates** the workspace's binding
   before publication releases its reservation, because directory identity
   alone cannot detect content replaced inside an existing storage directory
   and inode numbers can be reused. A forced pass (`zg --index --reconcile`)
   invalidates the prior verification **when it starts**, so cancellation or
   failure leaves the index unverified rather than trusted; a genuine
   absent record is the only ignored case during invalidation. Manually
   restoring index files in place is **unsupported** and not detected; the
   documented recovery is the forced pass.
5. Acceptance: unchanged relocated content causes **zero** document-embedding
   calls. Query embedding is separate and expected.

## 6. Version gates and migration

1. `manifestVersion: 2`: version-1 executables reject it through the existing
   manifest validation with a clear error. A version-2 executable reading a
   version-1 manifest reports that the index needs migration; it does not
   silently reinterpret version-1 absolute-path records.
2. `indexVersion` is bumped for the new files-collection schema (CRP identity,
  no persisted absolute path). Old executables reject the new storage through
   the existing version check.
3. **Converter**: an explicit operation (`zg --migrate-index <legacy-home>
   <destination-root>`; the destination is required) that reads a **closed**
   version-1 index under a lock acquired before any read, validates the
   original source-root mapping, computes CRPs, and builds a version-2
   destination inside an **exclusive destination reservation** (see §7a):
   - full ID remapping (file IDs, fragment IDs, group references,
     `entity_ids_json` inventories) with the index UUID preserved — the
     accepted decision; no permanent lookup table;
   - fragment content preserved byte-exactly; stored vectors are exact for dot
     and Euclidean metrics and within two float32 ULPs per component for cosine
     round trips (reported separately from exact equality), with native
     write statuses checked;
   - verification of the destination before activation: counts, unique and
     **correctly derived** identities (`sha256hex(index UUID + "\0" +
     canonical path)` for files and the fragment rule for entities),
     per-file ownership, exact public inventories, single-file groups with
     exactly one owned major, required typed fields, and vector preservation
     (sampled checks are named as such);
   - every opened handle is closed in exception-safe finalizers; the source
     is never modified; an interrupted or rejected conversion leaves no
     staging residue and the source usable;
   - the migrated index starts **unverified** and reconciles at its first
     indexing run, with verification explicitly invalidated at publication.

### 7a. Destination reservation protocol

Migration, import, and export publish through one protocol:

1. **Reserve at start**: create the destination and hold its write lock for
   the whole operation. Validation happens **under** the lock: a destination
   containing index markers fails; a destination containing anything other
   than lock scaffolding is rejected as unrelated contents and preserved
   untouched. An abandoned reservation is never reclaimed automatically: it
   stays blocked until the documented operator recovery (point 7).
2. **Build inside the reservation** and verify the staged result. Import
   additionally holds the artifact's read lock while consuming it, so
   consumers respect the exporter's release-as-commit boundary. Export,
   import, and migration all run the incomplete-home guard under the source
   lock before any source read.
3. **Re-verify ownership before publication and before any cleanup**: the
   home's device/inode, the write lock's device/inode, and the operation's
   token must all match. Ownership loss fences the operation; nothing is
   merged, published, or deleted on someone else's behalf.
4. **Publish without overwriting**: finalize the manifest into staging, then
   move staged children (manifest last); a child-name collision is an
   explicit error and the foreign child is never replaced. Each moved child
   is recorded with the identity it had in staging; rollback compares against
   that recorded identity, never adopts a post-move observation as ownership,
   and never overwrites content found at a staging target.
5. **Commit once**: the operation commits only when its release **actually
   releases** — a failed release is ownership loss, leaves everything in
   place for operator review, and reports failure. After a successful
   release, no error path cleans the result.
6. **Pre-commit abort**: only provably owned staging is removed, after
   native handles are closed and only while ownership remains verifiable;
   the destination home itself is never recursively deleted. If rollback
   cannot complete, blockage is preserved: the INCOMPLETE marker while it
   stands, otherwise the write lock is retained as the last block, and the
   failure reports the actual state left and the required operator recovery.
7. **Durable incomplete state**: a reservation writes an `INCOMPLETE` marker
   into the destination at start and removes it only through a checked
   transition at commit — absence, replacement, or removal failure can never
   be converted into successful publication. Readers, discovery, and writers
   treat any home carrying the marker as an explicit error — never an
   ancestor fallback — across process death and lock cleanup. Recovery is
   the documented operator action: with writers quiescent, remove the marker
   and partial contents, then retry. Since write locks are never reclaimed
   automatically, an abandoned reservation blocks until that recovery.

## 7. Export and import (logical portability)

Logical export/import (`zg --export-index` / `zg --import-index`) is the
required route when native database files do not cross platforms. Export
works from a legacy (v1) or portable (v2) source; import needs only the
artifact, never the source database.

1. **Consistency**: the source is read under a lock acquired before any read,
   excluding writers across both collections and the manifest for the whole
   export.
2. **Completeness**: the entities collection is iterated directly (not through
   the public per-file inventory), so secondary fragments and their vectors
   are included.
3. **Artifact**: a versioned directory — `format.json` (format version,
   embedding schema, declared counts), `manifest.json` in portable v2 form
   (no absolute location, no device, no API key, no verification claim), and
   `files.jsonl` / `entities.jsonl` with every scalar field, fragment
   content, relationships, and vectors (base64).
4. **Import validation**: the artifact manifest is validated against the
   normal reader contract and reconstructed from an allowlist before anything
   is staged (credential-bearing, legacy, malformed, or host-bound metadata
   is rejected); artifact and manifest must agree on identity, embedding
   schema, and supported versions; invalid artifacts leave any existing
   destination untouched.
5. **Import**: build inside the destination reservation (§7a), insert with
   status checks, and run the same verification as migration — counts, unique
   and correctly derived identities, ownership, exact inventories, group
   integrity, required fields, vector preservation under the metric rules in §7.
   Import pays structure
   rebuild, never inference, and the imported index starts unverified with
   verification explicitly invalidated at publication.
6. **Boundary proof**: import runs in a separate process with the source
   unavailable, reading only the serialized artifact; test results cross the
   process boundary as fresh, validated result files.

## 8. Test obligations

- Relocate A→B with A unavailable: search resolves every destination under B;
  refresh produces zero document embeddings and a stable inventory.
- A and B coexist with different contents: operations on B never read or
  modify A.
- Timestamp-only changes: existing vectors reused.
- Changed content with unchanged size/timestamp: detected by the
  reconciliation pass (the test restores the indexed stat values captured
  before the edit).
- Edit, add, delete, rename: correct incremental behavior without stale or
  orphaned records.
- Multilingual filenames, NFD/NFC forms, case collisions: correct resolution
  or explicit rejection.
- Containment through storage and search operations, not only the resolver:
  saved roots or directories swapped for escaping symlinks are explicit
  errors; impostor files sharing an indexed name are never served; ignore
  files that escape are rejected.
- Symlink escaping the workspace: exclusion with diagnostic.
- Two live copies sharing an index UUID: concurrent use without shared locks
  or handles.
- Old executable vs new format and new executable vs old format: clear
  rejection/migration guidance.
- Conversion and import: vectors and content preserved, relationships intact
  (ownership, exact inventories, single-file groups, unique identities),
  source untouched after interruption (no leaked descriptors, no staging
  residue) or invalid input, successful retry, corrupt relationship graphs
  rejected before activation.
- Transfer artifacts contain no credential material and no verification
  claim; imported/migrated indexes start unverified and reconcile.
- Import in a separate process with the source unavailable, reading only the
  artifact, with asserted zero inference.
- Optional: native macOS→Linux open probe. Not required while the logical
  route passes.

## 9. Non-goals for version 1

Scan roots outside the workspace; rename optimization; Windows-specific
validation (the contract is Windows-compatible by construction); upstream
acceptance.
