use super::{Direction, Result, SqliteGraphStorage, decode_enum, decode_metadata, nonempty};
use crate::domain::{FileEdge, GraphEdgeKind};
use rusqlite::{Row, params_from_iter};

impl SqliteGraphStorage {
    /// Stream resolved one-hop edges from one SQL execution, without ordering them.
    /// The callback may stop before exhausting the cursor.
    /// `None` selects all kinds; `Some(&[])` selects none.
    /// Self-loops appear once; distinct stored call sites are preserved.
    ///
    /// # Errors
    /// Rejects blank IDs, invalid stored data, and SQLite errors.
    pub(crate) fn scan_neighborhood<T>(
        &self,
        id: &str,
        direction: Direction,
        kinds: Option<&[GraphEdgeKind]>,
        read: impl FnOnce(&mut dyn Iterator<Item = Result<FileEdge>>) -> T,
    ) -> Result<T> {
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
                return Ok(read(&mut std::iter::empty()));
            }
            let mut names: Vec<&str> = kinds.iter().map(|kind| kind.as_str()).collect();
            names.sort_unstable();
            names.dedup();
            sql.push_str(" AND kind IN (");
            sql.push_str(&vec!["?"; names.len()].join(", "));
            sql.push(')');
            values.extend(names);
        }
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement
            .query_map(params_from_iter(values), edge_from_row)?
            .map(|row| row.map_err(super::Error::from));
        Ok(read(&mut rows))
    }

    #[cfg(test)]
    pub(crate) fn neighborhood(
        &self,
        id: &str,
        direction: Direction,
        kinds: Option<&[GraphEdgeKind]>,
    ) -> Result<Vec<FileEdge>> {
        self.scan_neighborhood(id, direction, kinds, |rows| rows.collect())?
    }
}

fn edge_from_row(row: &Row<'_>) -> rusqlite::Result<FileEdge> {
    Ok(FileEdge {
        kind: decode_enum(row.get(0)?)?,
        source: row.get(1)?,
        target: row.get(2)?,
        line: row.get(3)?,
        column: row.get(4)?,
        provenance: decode_enum(row.get(5)?)?,
        metadata: decode_metadata(&row.get::<_, String>(6)?)?,
    })
}
