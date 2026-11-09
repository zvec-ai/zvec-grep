use crate::domain::{EdgeProvenance, FileGraphResult, GraphEdgeKind, PendingRefStatus};
use rusqlite::{TransactionBehavior, params, params_from_iter};
use std::collections::HashSet;

use super::{Error, Result, SqliteGraphStorage, nonempty};

// Bound IN-clause parameters; invalidation also binds the owning file ID.
const TARGET_ID_BATCH_SIZE: usize = 512;

impl SqliteGraphStorage {
    /// Atomically replaces one file's local graph rows and invalidates inbound
    /// edges.
    /// `old_entity_ids` must contain **all** pre-update entity IDs from zvec; pass
    /// an empty slice for a new file. Snapshot node IDs validate ownership.
    /// The implicit file endpoint is `f{file_id}`, matching
    /// the zvec file document key. Cross-file edges are written by the resolver.
    /// Invalidation here is bounded by endpoint identity; the workspace-wide
    /// evidence resolution also reads (name candidates, module paths) is
    /// recorded in the same transaction and requeued once per pass through
    /// `IndexStore::requeue_stale_resolutions`.
    /// `paths_changed` reports that this write made the file appear or
    /// disappear, which can reorder the preferred path variant of every import.
    /// A file whose extends/implements clauses change also stales every
    /// member call that resolved through that chain, which no symbol it owns
    /// names; the write records that evidence as well.
    /// `language` labels pending references for the resolver's language gate;
    /// `None` records `unknown`, so the gate only keeps candidates that carry
    /// the same absent evidence.
    /// Read old IDs before mutating zvec, under the same workspace write lock.
    /// This transaction covers SQLite only; the coordinator owns cross-store recovery.
    ///
    /// # Errors
    /// Rejects invalid ownership, duplicate/empty IDs, non-local edges, invalid
    /// positions, read-only connections, and SQLite failures. Failures roll back.
    pub(crate) fn write_file_graph(
        &mut self,
        file_id: u32,
        language: Option<&str>,
        graph: &FileGraphResult,
        old_entity_ids: &[String],
        paths_changed: bool,
    ) -> Result<()> {
        let language = language.unwrap_or("unknown");
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
            // and reference evidence for the next resolution pass, but discard
            // candidates recorded against the previous snapshot.
            tx.execute(
                &format!(
                    "UPDATE edges SET status = 'pending', target = NULL, provenance = NULL, candidates = NULL
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
        // Taken before the rows are replaced: the old rows name the bases this
        // file leaves behind, the new output the bases it joins. Only an actual
        // change stales the workspace's hierarchy-bound calls; merely owning an
        // extends clause, as most class files do, must not requeue them on
        // every save.
        let hierarchy_changed = inheritance_edges(&tx, file_id)? != inheritance_evidence(graph);
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
                (file_id, source, reference_name, receiver_name, kind, arity, line, col, language, name_tail, status, metadata)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?)")?;
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
                    language,
                    name_tail(&reference.ref_name),
                    serde_json::to_string(&reference.metadata)?
                ])?;
            }
        }
        // Recorded with the rows it can stale, not once per pass: a pass that
        // publishes this file and then fails or is cancelled before its sweep
        // would otherwise lose the only record that earlier resolutions are
        // stale, and no later pass revisits them.
        Self::record_stale_evidence(
            &tx,
            paths_changed,
            hierarchy_changed,
            graph.nodes.iter().filter_map(|node| node.name.as_deref()),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Deletes owned rows and invalidates incoming references, including imports
    /// targeting the file itself. Repeating deletion is safe. Deletion carries no
    /// language evidence, so it records `unknown`.
    ///
    /// # Errors
    /// Rejects empty IDs and SQLite failures. The complete mutation is atomic.
    pub(crate) fn delete_file_graph(
        &mut self,
        file_id: u32,
        old_entity_ids: &[String],
    ) -> Result<()> {
        // A vanished file can change which module path an import prefers.
        self.write_file_graph(
            file_id,
            None,
            &FileGraphResult::default(),
            old_entity_ids,
            true,
        )
    }
}

/// Last component of a referenced name, matching the name index's namespace:
/// `a::b::c`, `a.b.c` and `a/b/c` all index as `c`.
fn name_tail(name: &str) -> &str {
    name.rsplit([':', '.', '/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(name)
}

/// Whether a relationship kind spells a base this file inherits from.
fn spells_inheritance(kind: GraphEdgeKind) -> bool {
    matches!(kind, GraphEdgeKind::Extends | GraphEdgeKind::Implements)
}

/// The inheritance evidence a file's new output carries, one `(kind, source,
/// base)` triple per clause: a resolved edge names its target, a pending one
/// the reference it spells. Sorted and deduplicated so two writes of the same
/// clauses compare equal.
fn inheritance_evidence(graph: &FileGraphResult) -> Vec<(String, String, String)> {
    let mut evidence: Vec<(String, String, String)> = graph
        .edges
        .iter()
        .filter(|edge| spells_inheritance(edge.kind))
        .map(|edge| {
            (
                edge.kind.as_str().to_owned(),
                edge.source.clone(),
                edge.target.clone(),
            )
        })
        .collect();
    evidence.extend(
        graph
            .pending_refs
            .iter()
            .filter(|reference| spells_inheritance(reference.ref_kind))
            .map(|reference| {
                (
                    reference.ref_kind.as_str().to_owned(),
                    reference.owner_id.clone(),
                    reference.ref_name.clone(),
                )
            }),
    );
    evidence.sort();
    evidence.dedup();
    evidence
}

/// The inheritance evidence this file's stored rows carry now, read the same
/// way as `inheritance_evidence` and therefore by spelling: a reference keeps
/// the name it spells after it resolves, and a directly written edge names its
/// target. Runs before the rows are replaced.
fn inheritance_edges(
    tx: &rusqlite::Transaction<'_>,
    file_id: u32,
) -> Result<Vec<(String, String, String)>> {
    let mut statement = tx.prepare(
        "SELECT kind, source, COALESCE(reference_name, target) FROM edges
         WHERE file_id = ?1 AND kind IN ('extends', 'implements')",
    )?;
    let rows = statement.query_map([file_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
    let mut evidence = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    evidence.sort();
    evidence.dedup();
    Ok(evidence)
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
