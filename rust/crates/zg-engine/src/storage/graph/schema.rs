use rusqlite::{Connection, TransactionBehavior};

use super::{Error, Result};

// Unsupported databases require an index rebuild; no schema migration is provided.
// Version compatibility belongs to the workspace manifest, which rebuilds the whole
// index when the index version changes; this database only proves ownership.
// The edges table carries both the walk-time edges and the resolver's data plane
// (ambiguity candidates, reference language and name tail); names, ownership and
// paths stay canonical in zvec.
pub(crate) const APPLICATION_ID: i64 = 0x5a47_5250;

pub(crate) fn validate(connection: &Connection) -> Result<()> {
    let application_id: i64 =
        connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    if application_id != APPLICATION_ID {
        return Err(Error::UnsupportedSchema { application_id });
    }
    let edges: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'edges'",
        [],
        |row| row.get(0),
    )?;
    if edges != 1 {
        return Err(Error::IncompleteSchema);
    }
    Ok(())
}

pub(crate) fn initialize(connection: &mut Connection) -> Result<()> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let application_id: i64 = tx.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    if application_id == 0 {
        let objects: i64 = tx.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'",
            [],
            |row| row.get(0),
        )?;
        if objects != 0 {
            return Err(Error::ForeignDatabase);
        }
        tx.execute_batch(SCHEMA)?;
        tx.pragma_update(None, "application_id", APPLICATION_ID)?;
    } else {
        validate(&tx)?;
    }
    // Indexes are additive, so an existing database keeps its rows and gains
    // the ones it was built without. A read-only open cannot write and only
    // loses the index acceleration; the next writable open catches up.
    tx.execute_batch(INDEXES)?;
    tx.execute_batch(INVALIDATION)?;
    tx.commit()?;
    Ok(())
}

// Direct local edges and references share one table. Resolution fills the target
// on the existing reference row; invalidation clears it and retains the evidence.
// `failed` terminates import references whose specifier addresses modules
// outside the workspace; they carry no target, so endpoint invalidation cannot
// revive them. Rewriting the owning file re-enqueues them, and so does a later
// pass that requeues the evidence they depended on.
const SCHEMA: &str = "
CREATE TABLE edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id INTEGER NOT NULL CHECK (file_id BETWEEN 0 AND 4294967295),
    source TEXT NOT NULL,
    target TEXT,
    kind TEXT NOT NULL CHECK (kind IN ('contains', 'calls', 'imports', 'extends', 'implements', 'declares')),
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'resolved', 'failed')),
    reference_name TEXT,
    receiver_name TEXT,
    arity INTEGER CHECK (arity >= 0),
    line INTEGER CHECK (line >= 1),
    col INTEGER CHECK (col >= 0),
    candidates TEXT CHECK (candidates IS NULL OR (json_valid(candidates) AND json_type(candidates) = 'array')),
    language TEXT NOT NULL DEFAULT 'unknown',
    name_tail TEXT NOT NULL DEFAULT '',
    provenance TEXT CHECK (provenance IN ('arity_bound', 'export_bound', 'file_local', 'file_scoped', 'import_bound', 'import_scoped', 'qualified_name_match', 'workspace_unique')),
    metadata TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata) AND json_type(metadata) = 'object'),
    CHECK ((status = 'resolved' AND target IS NOT NULL AND provenance IS NOT NULL)
        OR (status IN ('pending', 'failed') AND target IS NULL AND provenance IS NULL)),
    CHECK (
        (reference_name IS NULL AND status = 'resolved' AND provenance = 'file_local'
            AND receiver_name IS NULL AND arity IS NULL AND candidates IS NULL)
        OR
        (reference_name IS NOT NULL AND length(trim(reference_name)) > 0
            AND line IS NOT NULL AND col IS NOT NULL
            AND (provenance IS NULL OR provenance <> 'file_local'))
    )
) STRICT;
";

// Created on every writable open, not only when the table is: an index is
// derived data, so a database built by an older binary gains the ones added
// since without a rebuild. A database whose columns predate one of these
// indexes fails here with `no such column`, which is the same rebuild signal
// the first read of that column would have produced.
const INDEXES: &str = "
CREATE INDEX IF NOT EXISTS edges_file ON edges(file_id);
CREATE INDEX IF NOT EXISTS edges_source_kind ON edges(source, kind);
CREATE INDEX IF NOT EXISTS edges_target_kind ON edges(target, kind);
-- Index only the pending queue so resolved queries use the endpoint indexes.
CREATE INDEX IF NOT EXISTS edges_pending_id ON edges(id) WHERE status = 'pending';
-- The incremental requeue probes this column by value on every pass. The
-- partial predicate repeats the probe's `reference_name IS NOT NULL` verbatim,
-- because SQLite only considers a partial index when the query implies its
-- predicate; it also leaves out `contains` edges and the `file_local` rows that
-- the CHECK constraint pins to a NULL `reference_name`.
CREATE INDEX IF NOT EXISTS edges_name_tail ON edges(name_tail)
    WHERE reference_name IS NOT NULL;
";

// Evidence a pass recorded but has not requeued yet. It lives in the database
// rather than in memory so that a pass which publishes files and then fails or
// is cancelled before its sweep still repairs those resolutions on the next
// open. Created on every writable open like the indexes, so a database built
// without the table gains it without a rebuild.
const INVALIDATION: &str = "
CREATE TABLE IF NOT EXISTS zg_stale_names (
    kind INTEGER NOT NULL CHECK (kind IN (0, 1)),
    name TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (kind, name)
) STRICT;
";
