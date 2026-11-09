use rusqlite::{Transaction, TransactionBehavior};

use super::{Result, SqliteGraphStorage};

/// `zg_stale_names.kind` of one symbol name the pass wrote.
const STALE_NAME: i64 = 0;
/// `zg_stale_names.kind` of one structural change. `name` distinguishes the
/// kinds: the table already carried this `kind` when inheritance evidence was
/// added, and a wider `CHECK` would not reach an existing database.
const STALE_STRUCTURE: i64 = 1;
/// A file appeared or disappeared, which can reorder the module paths an
/// import specifier reads.
const STRUCTURE_PATHS: &str = "";
/// A file joined or left an inheritance chain, which changes the member a
/// receiver call resolves to without changing any name it spells.
const STRUCTURE_HIERARCHY: &str = "hierarchy";

/// Name budget for one pass's requeue. A larger recorded set falls back to the
/// blanket requeue, which needs no name list and is the cheaper of the two
/// then.
const MAX_STALE_NAMES: usize = 4096;

impl SqliteGraphStorage {
    /// Records one file write's effect on the workspace-wide sets resolution
    /// reads, inside the caller's transaction.
    ///
    /// Endpoint invalidation in `write_file_graph` only revisits rows whose old
    /// target belonged to the replaced file. Resolution is also a function of
    /// two workspace-wide sets: the module paths an import specifier can name
    /// (`path_variants`), and the entities sharing a name with a reference.
    /// Adding or replacing files changes both without touching any existing
    /// endpoint, so the evidence is written with the graph rows themselves: a
    /// pass that never reaches its sweep still leaves it for the next one.
    ///
    /// `hierarchy_changed` covers a third input that names cannot express: the
    /// extends/implements chain a receiver call resolves through. Swapping one
    /// file's base class leaves every member call bound through that chain
    /// pointing at the old declaration, and neither the call nor its obsolete
    /// target spells any name the write recorded.
    pub(super) fn record_stale_evidence<'a>(
        tx: &Transaction<'_>,
        paths_changed: bool,
        hierarchy_changed: bool,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<()> {
        let mut insert =
            tx.prepare("INSERT OR IGNORE INTO zg_stale_names (kind, name) VALUES (?1, ?2)")?;
        if paths_changed {
            insert.execute((STALE_STRUCTURE, STRUCTURE_PATHS))?;
        }
        if hierarchy_changed {
            insert.execute((STALE_STRUCTURE, STRUCTURE_HIERARCHY))?;
        }
        for name in names {
            insert.execute((STALE_NAME, name))?;
        }
        Ok(())
    }

    /// Returns references whose evidence the recorded writes changed to the
    /// pending queue, and consumes the record.
    ///
    /// The resolution sweep only reads `pending` rows, so a resolved reference
    /// never sees a changed candidate set or module path again unless a pass
    /// explicitly requeues it. A path change marks a pass that added or removed
    /// a file, which can reorder the preferred `path_variants` entry of every
    /// import; import-scoped calls follow it because they bind through the
    /// file's resolved imports. The recorded names hold every symbol this pass
    /// wrote, because a second entity of the same name can break a former
    /// unique match. A hierarchy change reaches a third set: member calls whose
    /// declaring scope was chosen by walking extends/implements, which no name
    /// in this pass spells. `overflow` replaces an oversized name set with a
    /// blanket requeue of the same evidence.
    ///
    /// `failed` rows requeue too where a changed set can make their specifier
    /// resolvable. Rows with a NULL `reference_name` are `file_local` edges
    /// resolved during the walk, which never consult these sets and stay
    /// untouched.
    ///
    /// The caller must serialize this pass with file writes, exactly like the
    /// resolution writeback.
    ///
    /// # Errors
    /// Rejects SQLite failures. Consuming the record happens in the same
    /// transaction as the requeue, so a failed attempt keeps the evidence for
    /// the next pass instead of losing the retry intent.
    pub(crate) fn requeue_stale_resolutions(&mut self) -> Result<usize> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let recorded: i64 = tx.query_row(
            "SELECT count(*) FROM zg_stale_names WHERE kind = ?1",
            [STALE_NAME],
            |row| row.get(0),
        )?;
        let paths_changed = structure_recorded(&tx, STRUCTURE_PATHS)?;
        let hierarchy_changed = structure_recorded(&tx, STRUCTURE_HIERARCHY)?;
        if recorded == 0 && !paths_changed && !hierarchy_changed {
            return Ok(0);
        }
        let overflow = usize::try_from(recorded).unwrap_or(usize::MAX) > MAX_STALE_NAMES;
        let mut requeued = 0;
        if paths_changed || overflow {
            let predicate = if overflow {
                ""
            } else {
                " AND (kind = 'imports' OR provenance IN ('import_bound', 'import_scoped'))"
            };
            requeued += tx.execute(
                &format!(
                    "UPDATE edges SET status = 'pending', target = NULL, provenance = NULL, candidates = NULL
                     WHERE status IN ('resolved', 'failed') AND reference_name IS NOT NULL{predicate}"
                ),
                [],
            )?;
        }
        if hierarchy_changed && !overflow {
            // `bind_nearest_member` stamps every hierarchy-bound member call
            // `file_scoped`; a row without a receiver never read the chain, so
            // it stays bound and out of this sweep.
            requeued += tx.execute(
                "UPDATE edges SET status = 'pending', target = NULL, provenance = NULL, candidates = NULL
                 WHERE status = 'resolved' AND provenance = 'file_scoped' AND receiver_name IS NOT NULL",
                [],
            )?;
        }
        if recorded > 0 && !overflow {
            // Two statements rather than one `OR`, so the name probe seeks the
            // partial index on `name_tail`. SQLite answers an `OR` with a scan
            // of a single index, and the receiver arm has no index to merge.
            // The receiver arm is the rare one: it requeues rows whose entity
            // name only appears as the receiver of a member reference.
            requeued += tx.execute(
                "UPDATE edges SET status = 'pending', target = NULL, provenance = NULL, candidates = NULL
                 WHERE status IN ('resolved', 'failed') AND reference_name IS NOT NULL
                   AND name_tail IN (SELECT name FROM zg_stale_names WHERE kind = ?1)",
                [STALE_NAME],
            )?;
            requeued += tx.execute(
                "UPDATE edges SET status = 'pending', target = NULL, provenance = NULL, candidates = NULL
                 WHERE status IN ('resolved', 'failed') AND reference_name IS NOT NULL
                   AND receiver_name IN (SELECT name FROM zg_stale_names WHERE kind = ?1)",
                [STALE_NAME],
            )?;
        }
        tx.execute("DELETE FROM zg_stale_names", [])?;
        tx.commit()?;
        Ok(requeued)
    }
}

/// Whether one structural change is recorded.
fn structure_recorded(tx: &Transaction<'_>, name: &str) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM zg_stale_names WHERE kind = ?1 AND name = ?2)",
        (STALE_STRUCTURE, name),
        |row| row.get(0),
    )?)
}
