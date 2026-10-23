use super::{
    Error, PendingRef, PendingRefPage, Provenance, Resolution, ResolutionStats, Result,
    SqliteGraphStorage, StoredPendingRef, decode_enum, decode_metadata, nonempty,
};
use rusqlite::{Row, TransactionBehavior, params};

const SELECT_REF: &str = "SELECT id, file_id, source, reference_name, receiver_name, kind, arity, line, col, metadata, candidates, language, name_tail FROM edges";

impl SqliteGraphStorage {
    /// Lists only pending refs using keyset pagination; no snapshot spans pages.
    /// Restart with cursor 0 after file mutations. Limit must be 1..=1000.
    /// # Errors
    /// Rejects invalid limits/cursors and malformed stored rows or SQLite errors.
    pub(crate) fn list_pending_refs(&self, limit: usize, cursor: i64) -> Result<PendingRefPage> {
        if !(1..=1000).contains(&limit) || cursor < 0 {
            return Err(Error::InvalidInput(
                "pending query requires limit 1..1000 and nonnegative cursor",
            ));
        }
        let mut statement = self.connection.prepare(&format!(
            "{SELECT_REF} WHERE status = 'pending' AND id > ? ORDER BY id LIMIT ?"
        ))?;
        let mut refs: Vec<_> = statement
            .query_map(params![cursor, limit + 1], ref_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        let more = refs.len() > limit;
        refs.truncate(limit);
        let next_cursor = if more {
            refs.last().map(|reference| reference.id)
        } else {
            None
        };
        Ok(PendingRefPage { refs, next_cursor })
    }

    /// Atomically applies proposals for existing pending refs, skipping missing
    /// or already resolved refs. The caller must serialize the entire read,
    /// resolution and writeback cycle with workspace writes/deletions.
    /// Validate targets in zvec before writeback. Row IDs alone cannot detect
    /// outdated proposals for references that were resolved and then invalidated.
    /// # Errors
    /// Rejects invalid proposals and SQLite failures; the entire batch rolls back.
    pub(crate) fn apply_resolutions(
        &mut self,
        resolutions: &[Resolution],
    ) -> Result<ResolutionStats> {
        for resolution in resolutions {
            nonempty(&resolution.target_id)?;
            if resolution.ref_id < 1 || resolution.provenance == Provenance::FileLocal {
                return Err(Error::InvalidInput(
                    "resolution requires a positive ref ID and cross-file provenance",
                ));
            }
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut stats = ResolutionStats::default();
        {
            let mut update = tx.prepare(
                "UPDATE edges SET target = ?, provenance = ?, status = 'resolved'
                 WHERE id = ? AND status = 'pending'",
            )?;
            for resolution in resolutions {
                let changed = update.execute(params![
                    resolution.target_id,
                    resolution.provenance.as_str(),
                    resolution.ref_id,
                ])?;
                if changed == 0 {
                    stats.stale += 1;
                } else {
                    stats.resolved += 1;
                }
            }
        }
        tx.commit()?;
        Ok(stats)
    }

    /// Atomically records ambiguous-name candidate sets on pending refs so
    /// consumers can inspect who a reference might target. Rows already
    /// resolved or invalidated are skipped; the same caller serialization as
    /// `apply_resolutions` applies.
    ///
    /// # Errors
    /// Rejects invalid proposals and SQLite failures; the entire batch rolls back.
    pub(crate) fn record_candidates(&mut self, entries: &[(i64, Vec<String>)]) -> Result<usize> {
        for (ref_id, candidates) in entries {
            if *ref_id < 1 || candidates.is_empty() {
                return Err(Error::InvalidInput(
                    "candidate sets require a positive ref ID and at least one candidate",
                ));
            }
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut recorded = 0;
        {
            let mut update =
                tx.prepare("UPDATE edges SET candidates = ? WHERE id = ? AND status = 'pending'")?;
            for (ref_id, candidates) in entries {
                let payload = serde_json::to_string(candidates)?;
                let changed = update.execute(params![payload, ref_id])?;
                recorded += changed;
            }
        }
        tx.commit()?;
        Ok(recorded)
    }

    /// Atomically terminates pending references whose specifier cannot
    /// address any workspace module. Failed rows drop out of the queue until
    /// the owning file is rewritten; they carry no target, so invalidation
    /// never resurrects them. The same caller serialization as
    /// `apply_resolutions` applies.
    ///
    /// # Errors
    /// Rejects invalid proposals and SQLite failures; the entire batch rolls back.
    #[allow(dead_code)]
    pub(crate) fn apply_failures(&mut self, ref_ids: &[i64]) -> Result<usize> {
        if ref_ids.iter().any(|ref_id| *ref_id < 1) {
            return Err(Error::InvalidInput("failures require a positive ref ID"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut failed = 0;
        {
            let mut update = tx.prepare(
                "UPDATE edges SET status = 'failed', candidates = NULL
                 WHERE id = ? AND status = 'pending'",
            )?;
            for ref_id in ref_ids {
                failed += update.execute(params![ref_id])?;
            }
        }
        tx.commit()?;
        Ok(failed)
    }
}

fn ref_from_row(row: &Row<'_>) -> rusqlite::Result<StoredPendingRef> {
    Ok(StoredPendingRef {
        id: row.get(0)?,
        file_id: row.get(1)?,
        reference: PendingRef {
            from_node_id: row.get(2)?,
            reference_name: row.get(3)?,
            receiver_name: row.get(4)?,
            reference_kind: decode_enum(row.get(5)?)?,
            arity: row.get(6)?,
            line: row.get(7)?,
            col: row.get(8)?,
            metadata: decode_metadata(&row.get::<_, String>(9)?)?,
            candidates: row
                .get::<_, Option<String>>(10)?
                .map(|value| {
                    serde_json::from_str(&value).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            10,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                })
                .transpose()?,
            language: row.get(11)?,
            name_tail: row.get(12)?,
        },
    })
}
