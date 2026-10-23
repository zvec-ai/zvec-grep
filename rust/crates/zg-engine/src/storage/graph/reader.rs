use super::{
    DeclaredBinding, Direction, Edge, EdgeKind, ImportReference, Metadata, Result,
    SqliteGraphStorage, decode_enum, decode_metadata, nonempty,
};
use rusqlite::{Row, params_from_iter};
use serde_json::Value;

impl SqliteGraphStorage {
    /// All resolved one-hop edges in insertion order, with no result limit.
    /// `None` selects all kinds; `Some(&[])` selects none.
    /// Self-loops appear once; distinct stored call sites are preserved.
    /// Awaits read-side consumers (query pipelines, MCP tools); tests use it.
    ///
    /// # Errors
    /// Rejects blank IDs, invalid stored data, and SQLite errors.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn neighborhood(
        &self,
        id: &str,
        direction: Direction,
        kinds: Option<&[EdgeKind]>,
    ) -> Result<Vec<Edge>> {
        nonempty(id)?;
        let endpoint = match direction {
            Direction::In => "target = ?1",
            Direction::Out => "source = ?1",
            Direction::Both => "(source = ?1 OR target = ?1)",
        };
        let mut sql = format!(
            "SELECT kind, source, target, line, col, provenance, metadata
             FROM edges WHERE {endpoint} AND status = 'resolved'"
        );
        let mut values = vec![id];
        if let Some(kinds) = kinds {
            if kinds.is_empty() {
                return Ok(Vec::new());
            }
            let mut names: Vec<&str> = kinds.iter().map(|kind| kind.as_str()).collect();
            names.sort_unstable();
            names.dedup();
            sql.push_str(" AND kind IN (");
            sql.push_str(&vec!["?"; names.len()].join(", "));
            sql.push(')');
            values.extend(names);
        }
        sql.push_str(" ORDER BY id");
        let mut statement = self.connection.prepare(&sql)?;
        Ok(statement
            .query_map(params_from_iter(values), edge_from_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Entity IDs owned by a file in the last persisted snapshot, recovered
    /// from its `contains` edges. This is not the complete canonical ID set;
    /// update coordination must read IDs from zvec instead.
    /// Returns an empty set for files that were never persisted.
    ///
    /// # Errors
    /// Propagates SQLite errors.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn file_entity_ids(&self, file_id: u32) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT target FROM edges
             WHERE file_id = ?1 AND kind = 'contains' AND target IS NOT NULL
             ORDER BY target",
        )?;
        let rows = statement.query_map([file_id], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
    }

    /// Import statements of a file in insertion order, both pending and
    /// resolved: the resolver parses alias bindings from their statement
    /// text regardless of whether the specifier has been resolved yet.
    ///
    /// # Errors
    /// Propagates SQLite errors and invalid stored metadata.
    #[allow(dead_code)]
    pub(crate) fn import_references(&self, file_id: u32) -> Result<Vec<ImportReference>> {
        let mut statement = self.connection.prepare(
            "SELECT reference_name, metadata FROM edges
             WHERE file_id = ?1 AND kind = 'imports' ORDER BY id",
        )?;
        let rows = statement.query_map([file_id], |row| {
            let specifier: String = row.get(0)?;
            let metadata: Metadata = decode_metadata(&row.get::<_, String>(1)?)?;
            let raw_text = metadata.get("rawText").and_then(Value::as_str);
            Ok(ImportReference {
                specifier,
                raw_text: raw_text.map(str::to_owned),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The type bindings one entity declares (`var x = new X()` inside its
    /// body, typed parameters in its signature): each names the receiver
    /// type the resolver may infer when a call names the bound name.
    ///
    /// # Errors
    /// Propagates SQLite errors.
    #[allow(dead_code)]
    pub(crate) fn declared_bindings(&self, owner_id: &str) -> Result<Vec<DeclaredBinding>> {
        nonempty(owner_id)?;
        let mut statement = self.connection.prepare(
            "SELECT reference_name, metadata, line FROM edges
             WHERE source = ?1 AND kind = 'declares' ORDER BY line, id",
        )?;
        let rows = statement.query_map([owner_id], |row| {
            let name: String = row.get(0)?;
            let metadata: Metadata = decode_metadata(&row.get::<_, String>(1)?)?;
            let bound_type = metadata
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let line: i64 = row.get(2)?;
            Ok(DeclaredBinding {
                name,
                bound_type: bound_type.to_owned(),
                line: u32::try_from(line).unwrap_or_default(),
            })
        })?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<DeclaredBinding>>>()?
            .into_iter()
            .filter(|binding| !binding.bound_type.is_empty())
            .collect())
    }

    /// Inheritance evidence of one class entity: the base entity IDs whose
    /// extends/implements edges have resolved, plus the base names still
    /// pending. The resolver merges both views so a receiver call resolves
    /// in the same sweep that resolves the class's own inheritance edges.
    ///
    /// # Errors
    /// Propagates SQLite errors.
    #[allow(dead_code)]
    pub(crate) fn inheritance_bases(&self, entity_id: &str) -> Result<(Vec<String>, Vec<String>)> {
        nonempty(entity_id)?;
        let mut statement = self.connection.prepare(
            "SELECT target, reference_name, status FROM edges
             WHERE source = ?1 AND kind IN ('extends', 'implements') ORDER BY id",
        )?;
        let rows = statement.query_map([entity_id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut resolved = Vec::new();
        let mut pending = Vec::new();
        for row in rows {
            // Same-file bases resolve in the partition, whose resolved edges
            // spell the base in `target` alone; cross-file bases stay pending
            // with their spelled `reference_name`.
            let (target, base_name, status) = row?;
            if status == "resolved" {
                if let Some(target) = target {
                    resolved.push(target);
                }
            } else if let Some(base_name) = base_name {
                pending.push(base_name);
            }
        }
        Ok((resolved, pending))
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn edge_from_row(row: &Row<'_>) -> rusqlite::Result<Edge> {
    Ok(Edge {
        kind: decode_enum(row.get(0)?)?,
        source: row.get(1)?,
        target: row.get(2)?,
        line: row.get(3)?,
        column: row.get(4)?,
        provenance: decode_enum(row.get(5)?)?,
        metadata: decode_metadata(&row.get::<_, String>(6)?)?,
    })
}
