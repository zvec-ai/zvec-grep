use rusqlite::TransactionBehavior;

use super::{Result, SqliteGraphStorage};

impl SqliteGraphStorage {
    /// Returns resolved references whose evidence the current pass changed to
    /// the pending queue.
    ///
    /// Endpoint invalidation in `write_file_graph` only revisits rows whose old
    /// target belonged to the replaced file. Resolution is also a function of
    /// two workspace-wide sets: the module paths an import specifier can name
    /// (`path_variants`), and the entities sharing a name with a reference.
    /// Adding or replacing files changes both without touching any existing
    /// endpoint, so those rows would otherwise keep their stale target forever.
    ///
    /// `imports_changed` marks a pass that added or removed a file, which can
    /// reorder the preferred path variant of every import; import-scoped calls
    /// follow it because they bind through the file's resolved imports. `names`
    /// holds every symbol name this pass wrote, because a second entity of the
    /// same name can break a former unique match. `overflow` replaces an
    /// oversized name list with a blanket requeue of the same evidence.
    ///
    /// `failed` rows requeue too: they only record that the specifier named no
    /// workspace module at the time, and a later pass can make it resolvable.
    /// Rows with a NULL `reference_name` are `file_local` edges resolved during
    /// the walk, which never consult these sets and stay untouched.
    ///
    /// The caller must serialize this pass with file writes, exactly like the
    /// resolution writeback.
    ///
    /// # Errors
    /// Rejects SQLite failures; the requeue is one transaction.
    pub(crate) fn requeue_stale_resolutions(
        &mut self,
        names: &[String],
        imports_changed: bool,
        overflow: bool,
    ) -> Result<usize> {
        if names.is_empty() && !imports_changed && !overflow {
            return Ok(0);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut requeued = 0;
        if imports_changed || overflow {
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
        if !names.is_empty() {
            // A temporary table keeps one statement regardless of how many
            // names a pass wrote; binding the names directly would need
            // repeated chunks, and every chunk rescans the table.
            tx.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS zg_stale_names (name TEXT PRIMARY KEY);
                 DELETE FROM zg_stale_names;",
            )?;
            {
                let mut insert =
                    tx.prepare("INSERT OR IGNORE INTO zg_stale_names (name) VALUES (?)")?;
                for name in names {
                    insert.execute([name])?;
                }
            }
            // Two statements rather than one `OR`, so the name probe seeks the
            // partial index on `name_tail`. SQLite answers an `OR` with a scan
            // of a single index, and the receiver arm has no index to merge.
            // The receiver arm is the rare one: it requeues rows whose entity
            // name only appears as the receiver of a member reference.
            requeued += tx.execute(
                "UPDATE edges SET status = 'pending', target = NULL, provenance = NULL, candidates = NULL
                 WHERE status IN ('resolved', 'failed') AND reference_name IS NOT NULL
                   AND name_tail IN (SELECT name FROM zg_stale_names)",
                [],
            )?;
            requeued += tx.execute(
                "UPDATE edges SET status = 'pending', target = NULL, provenance = NULL, candidates = NULL
                 WHERE status IN ('resolved', 'failed') AND reference_name IS NOT NULL
                   AND receiver_name IN (SELECT name FROM zg_stale_names)",
                [],
            )?;
        }
        tx.commit()?;
        Ok(requeued)
    }
}
