use rusqlite::{TransactionBehavior, params, params_from_iter};
use std::collections::HashSet;

use super::{Error, FileGraph, Provenance, Result, SqliteGraphStorage, nonempty};

// Bound IN-clause parameters; invalidation also binds the owning file ID.
const TARGET_ID_BATCH_SIZE: usize = 512;

impl SqliteGraphStorage {
    /// Atomically replaces one file's local graph and invalidates inbound edges.
    /// `old_entity_ids` must contain **all** pre-update entity IDs from zvec; pass
    /// an empty slice for a new file. Entity IDs in the new snapshot are not stored
    /// in a second node table. The implicit file endpoint is `f{file_id}`, matching
    /// the zvec file document key. Cross-file edges must use `apply_resolutions`.
    /// Read old IDs before mutating zvec, under the same workspace write lock.
    /// This transaction covers SQLite only; the coordinator owns cross-store recovery.
    ///
    /// # Errors
    /// Rejects invalid ownership, duplicate/empty IDs, non-local edges, invalid
    /// positions, read-only connections, and SQLite failures. Failures roll back.
    pub(crate) fn write_file_graph(
        &mut self,
        file_id: u32,
        graph: &FileGraph,
        old_entity_ids: &[String],
    ) -> Result<()> {
        let file_node_id = format!("f{file_id}");
        validate(&file_node_id, graph, old_entity_ids)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut targets: Vec<&str> = old_entity_ids.iter().map(String::as_str).collect();
        targets.push(&file_node_id);
        targets.sort_unstable();
        targets.dedup();
        for chunk in targets.chunks(TARGET_ID_BATCH_SIZE) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            // Even unchanged target IDs require re-resolution. Preserve the row ID
            // and reference context, but discard candidates from the old snapshot.
            tx.execute(
                &format!("UPDATE edges SET status = 'pending', target = NULL, provenance = NULL, candidates = NULL
                  WHERE target IN ({placeholders}) AND file_id <> ? AND reference_name IS NOT NULL"),
                params_from_iter(
                    chunk.iter().map(|id| rusqlite::types::Value::Text((*id).into()))
                        .chain(std::iter::once(rusqlite::types::Value::Integer(i64::from(file_id)))),
                ),
            )?;
            tx.execute(
                &format!("DELETE FROM edges WHERE target IN ({placeholders})"),
                params_from_iter(chunk.iter().copied()),
            )?;
        }
        tx.execute("DELETE FROM edges WHERE file_id = ?", [file_id])?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO edges
                (file_id, kind, source, target, line, col, provenance, metadata, status)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'resolved')",
            )?;
            for edge in &graph.edges {
                insert.execute(params![
                    file_id,
                    edge.kind.as_str(),
                    edge.source,
                    edge.target,
                    edge.line,
                    edge.column,
                    edge.provenance.as_str(),
                    serde_json::to_string(&edge.metadata)?
                ])?;
            }
            let mut insert = tx.prepare("INSERT INTO edges
                (file_id, source, reference_name, receiver_name, kind, arity, line, col, candidates, language, name_tail, status, metadata)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?)")?;
            for reference in &graph.pending_refs {
                insert.execute(params![
                    file_id,
                    reference.from_node_id,
                    reference.reference_name,
                    reference.receiver_name,
                    reference.reference_kind.as_str(),
                    reference.arity,
                    reference.line,
                    reference.col,
                    reference
                        .candidates
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()?,
                    reference.language,
                    reference.name_tail,
                    serde_json::to_string(&reference.metadata)?
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Deletes owned rows and invalidates incoming references, including imports
    /// targeting the file itself. Repeating deletion is safe.
    ///
    /// # Errors
    /// Rejects empty IDs and SQLite failures. The complete mutation is atomic.
    pub(crate) fn delete_file_graph(
        &mut self,
        file_id: u32,
        old_entity_ids: &[String],
    ) -> Result<()> {
        self.write_file_graph(file_id, &FileGraph::default(), old_entity_ids)
    }
}

fn validate(file_id: &str, graph: &FileGraph, old_entity_ids: &[String]) -> Result<()> {
    nonempty(file_id)?;
    for id in old_entity_ids {
        nonempty(id)?;
    }
    let mut local = HashSet::new();
    for id in &graph.entity_ids {
        nonempty(id)?;
        if id == file_id || !local.insert(id.as_str()) {
            return Err(Error::InvalidInput(
                "entity IDs must be unique and distinct from the file ID",
            ));
        }
    }
    let owns = |id: &str| id == file_id || local.contains(id);
    for edge in &graph.edges {
        if edge.provenance != Provenance::FileLocal || !owns(&edge.source) || !owns(&edge.target) {
            return Err(Error::InvalidInput(
                "file snapshots require local edges; use apply_resolutions for cross-file edges",
            ));
        }
        if edge.line == Some(0) {
            return Err(Error::InvalidInput("edge lines must be one-based"));
        }
    }
    for reference in &graph.pending_refs {
        nonempty(&reference.reference_name)?;
        if !owns(&reference.from_node_id) || reference.line == 0 {
            return Err(Error::InvalidInput(
                "pending refs require local ownership and one-based lines",
            ));
        }
    }
    Ok(())
}
