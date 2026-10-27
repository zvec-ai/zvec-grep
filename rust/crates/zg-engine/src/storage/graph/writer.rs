use crate::domain::{EdgeProvenance, FileGraphResult, PendingRefStatus};
use rusqlite::{TransactionBehavior, params, params_from_iter};
use std::collections::HashSet;

use super::{Error, Result, SqliteGraphStorage, nonempty};

// Bound IN-clause parameters; invalidation also binds the owning file ID.
const TARGET_ID_BATCH_SIZE: usize = 512;

impl SqliteGraphStorage {
    /// Atomically replaces one file's local graph and invalidates inbound edges.
    /// `old_entity_ids` must contain **all** pre-update entity IDs from zvec; pass
    /// an empty slice for a new file. Snapshot node IDs validate ownership.
    /// The implicit file endpoint is `f{file_id}`, matching
    /// the zvec file document key. Cross-file edges are written by the resolver.
    /// Read old IDs before mutating zvec, under the same workspace write lock.
    /// This transaction covers SQLite only; the coordinator owns cross-store recovery.
    ///
    /// # Errors
    /// Rejects invalid ownership, duplicate/empty IDs, non-local edges, invalid
    /// positions, read-only connections, and SQLite failures. Failures roll back.
    pub(crate) fn write_file_graph(
        &mut self,
        file_id: u32,
        graph: &FileGraphResult,
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
            // and reference evidence for the next resolution pass.
            tx.execute(
                &format!(
                    "UPDATE edges SET status = 'pending', target = NULL, provenance = NULL
                  WHERE target IN ({placeholders}) AND file_id <> ? AND reference_name IS NOT NULL"
                ),
                params_from_iter(
                    chunk
                        .iter()
                        .map(|id| rusqlite::types::Value::Text((*id).into()))
                        .chain(std::iter::once(rusqlite::types::Value::Integer(i64::from(
                            file_id,
                        )))),
                ),
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
                (file_id, source, reference_name, receiver_name, kind, arity, line, col, status, metadata)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?)")?;
            for reference in &graph.pending_refs {
                insert.execute(params![
                    file_id,
                    reference.owner_id,
                    reference.ref_name,
                    reference.receiver_name,
                    reference.ref_kind.as_str(),
                    reference.arity,
                    reference.line,
                    reference.column,
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
        self.write_file_graph(file_id, &FileGraphResult::default(), old_entity_ids)
    }
}

pub(super) fn validate(
    file_id: &str,
    graph: &FileGraphResult,
    old_entity_ids: &[String],
) -> Result<()> {
    for id in old_entity_ids {
        nonempty(id)?;
    }
    let mut local = HashSet::new();
    for node in &graph.nodes {
        let id = &node.id;
        nonempty(id)?;
        if id == file_id || !local.insert(id.as_str()) {
            return Err(Error::InvalidInput(
                "entity IDs must be unique and distinct from the file ID",
            ));
        }
    }
    let owns = |id: &str| id == file_id || local.contains(id);
    for edge in &graph.edges {
        if edge.provenance != EdgeProvenance::FileLocal
            || !owns(&edge.source)
            || !owns(&edge.target)
        {
            return Err(Error::InvalidInput(
                "file snapshots require local edges; cross-file edges must be resolved separately",
            ));
        }
        if edge.line == Some(0) {
            return Err(Error::InvalidInput("edge lines must be one-based"));
        }
    }
    for reference in &graph.pending_refs {
        if reference.status != PendingRefStatus::Pending {
            return Err(Error::InvalidInput(
                "file snapshots require pending references",
            ));
        }
        nonempty(&reference.ref_name)?;
        if !owns(&reference.owner_id) || reference.line == 0 {
            return Err(Error::InvalidInput(
                "pending refs require local ownership and one-based lines",
            ));
        }
    }
    Ok(())
}
