use crate::domain::{EdgeProvenance, FileGraphResult, PendingRefStatus};
use rusqlite::{TransactionBehavior, params, params_from_iter};
use std::collections::HashSet;

use super::{Error, Metadata, Result, SqliteGraphStorage, nonempty};

// Bound IN-clause parameters; invalidation also binds the owning file ID.
const TARGET_ID_BATCH_SIZE: usize = 512;

/// Metadata keys an inheritance clause carries from assembly. Entity IDs hash
/// the entity's text, so a method-body edit mints a new one; the owner's
/// qualified name and the base's spelling are the halves a rewrite repeats.
const INHERITANCE_OWNER_KEY: &str = "owner";
const INHERITANCE_BASE_KEY: &str = "base";

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
    /// names; so does a file that keeps its clauses while moving the import
    /// binding a base name reads. The write records both as the same evidence.
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
        let hierarchy_changed = inheritance_changed(&tx, file_id, graph)?;
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

/// Evidence text a clause carries, falling back to the endpoint for rows
/// written before the clause recorded its spelling.
fn clause_text<'a>(metadata: &'a Metadata, key: &str, fallback: &'a str) -> &'a str {
    metadata
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or(fallback)
}

/// The inheritance evidence a file's new output carries, one `(kind, owner,
/// base)` triple per clause and by declaration identity: the owning entity's
/// qualified name stands in for its ID, which hashes the entity's text and
/// moves whenever only a method body is edited, and the base's spelling stands
/// in for a resolved target, which is such an ID. Sorted and deduplicated so
/// two writes of the same clauses compare equal.
fn inheritance_evidence(graph: &FileGraphResult) -> Vec<(String, String, String)> {
    let mut evidence: Vec<(String, String, String)> = graph
        .edges
        .iter()
        .filter(|edge| edge.kind.spells_inheritance())
        .map(|edge| {
            (
                edge.kind.as_str().to_owned(),
                clause_text(&edge.metadata, INHERITANCE_OWNER_KEY, &edge.source).to_owned(),
                clause_text(&edge.metadata, INHERITANCE_BASE_KEY, &edge.target).to_owned(),
            )
        })
        .collect();
    evidence.extend(
        graph
            .pending_refs
            .iter()
            .filter(|reference| reference.ref_kind.spells_inheritance())
            .map(|reference| {
                (
                    reference.ref_kind.as_str().to_owned(),
                    clause_text(
                        &reference.metadata,
                        INHERITANCE_OWNER_KEY,
                        &reference.owner_id,
                    )
                    .to_owned(),
                    reference.ref_name.clone(),
                )
            }),
    );
    evidence.sort();
    evidence.dedup();
    evidence
}

/// The inheritance evidence this file's stored rows carry now, read the same
/// way as `inheritance_evidence` and therefore by declaration identity: a
/// reference keeps the name it spells after it resolves, a directly written
/// edge keeps the base assembly recorded beside it, and rows from before that
/// evidence existed fall back to their endpoints. Runs before the rows are
/// replaced.
fn inheritance_edges(
    tx: &rusqlite::Transaction<'_>,
    file_id: u32,
) -> Result<Vec<(String, String, String)>> {
    let mut statement = tx.prepare(
        "SELECT kind, source, COALESCE(reference_name, target), metadata FROM edges
         WHERE file_id = ?1 AND kind IN ('extends', 'implements')",
    )?;
    let rows = statement.query_map([file_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut evidence = Vec::new();
    for row in rows {
        let (kind, source, base, metadata) = row?;
        let metadata: Metadata = metadata
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_default();
        // A resolved clause rewrites its target to an entity ID, so the stored
        // spelling is the only half that survives the resolution.
        let spelled = clause_text(
            &metadata,
            INHERITANCE_BASE_KEY,
            base.as_deref().unwrap_or_default(),
        );
        evidence.push((
            kind,
            clause_text(&metadata, INHERITANCE_OWNER_KEY, &source).to_owned(),
            spelled.to_owned(),
        ));
    }
    evidence.sort();
    evidence.dedup();
    Ok(evidence)
}

/// Whether this write moves the inheritance a caller's bindings rest on.
///
/// The clause identity `(kind, owner, base)` covers a base that is renamed or
/// dropped, but not one that keeps its spelling while binding elsewhere: a
/// header rewritten from `using left::Base` to `using right::Base` still spells
/// `(extends, Mid, Base)`, while the base edge now names a different
/// declaration and every member call bound through the chain must follow it.
/// Only a file that owns an inheritance clause pays for the import comparison,
/// so editing an import in an ordinary file still changes nothing here.
fn inheritance_changed(
    tx: &rusqlite::Transaction<'_>,
    file_id: u32,
    graph: &FileGraphResult,
) -> Result<bool> {
    let previous = inheritance_edges(tx, file_id)?;
    let current = inheritance_evidence(graph);
    if previous != current {
        return Ok(true);
    }
    if current.is_empty() {
        return Ok(false);
    }
    Ok(import_bindings(tx, file_id)? != import_evidence(graph))
}

/// The import specifiers a file's new output spells: what a base name reads
/// when the clause resolves outside the file. Sorted and deduplicated, so two
/// writes of the same imports compare equal.
fn import_evidence(graph: &FileGraphResult) -> Vec<String> {
    let mut evidence: Vec<String> = graph
        .pending_refs
        .iter()
        .filter(|reference| reference.ref_kind.spells_import())
        .map(|reference| reference.ref_name.clone())
        .collect();
    evidence.sort();
    evidence.dedup();
    evidence
}

/// The import specifiers this file's stored rows spell now. A resolved row
/// keeps the reference name it queued with, so the comparison reads the same
/// half on both sides. Rows without one are `file_local` imports resolved
/// during the walk, which never bound a name from another module.
fn import_bindings(tx: &rusqlite::Transaction<'_>, file_id: u32) -> Result<Vec<String>> {
    let mut statement = tx.prepare(
        "SELECT reference_name FROM edges
         WHERE file_id = ?1 AND kind = 'imports' AND reference_name IS NOT NULL",
    )?;
    let rows = statement.query_map([file_id], |row| row.get::<_, String>(0))?;
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
