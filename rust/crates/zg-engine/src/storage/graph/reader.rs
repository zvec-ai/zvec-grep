use super::{
    Direction, Edge, EdgeKind, Result, SqliteGraphStorage, decode_enum, decode_metadata, nonempty,
};
use rusqlite::{Row, params_from_iter};

impl SqliteGraphStorage {
    /// All resolved one-hop edges in insertion order, with no result limit.
    /// `None` selects all kinds; `Some(&[])` selects none.
    /// Self-loops appear once; distinct stored call sites are preserved.
    ///
    /// # Errors
    /// Rejects blank IDs, invalid stored data, and SQLite errors.
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
}

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
