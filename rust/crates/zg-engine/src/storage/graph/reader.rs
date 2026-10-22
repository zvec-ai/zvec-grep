use super::{Direction, Result, SqliteGraphStorage, decode_enum, decode_metadata, nonempty};
use crate::domain::{FileEdge, GraphEdgeKind};
use rusqlite::{Row, params_from_iter, types::Value};

impl SqliteGraphStorage {
    /// Keep all query pages in one SQLite snapshot. Dropping the guard also rolls back on panic.
    pub(crate) fn with_read_snapshot<T>(&self, read: impl FnOnce(&Self) -> T) -> Result<T> {
        let transaction = self.connection.unchecked_transaction()?;
        let result = read(self);
        transaction.rollback()?;
        Ok(result)
    }
    /// A bounded page of resolved edges in insertion order, after `cursor`.
    /// Return row IDs so callers can advance past invisible endpoints.
    /// `None` selects all kinds; `Some(&[])` selects none.
    /// Self-loops appear once; distinct stored call sites are preserved.
    ///
    /// # Errors
    /// Rejects blank IDs, invalid stored data, and SQLite errors.
    pub(crate) fn neighborhood_page(
        &self,
        id: &str,
        direction: Direction,
        kinds: Option<&[GraphEdgeKind]>,
        cursor: i64,
        limit: usize,
    ) -> Result<Vec<(i64, FileEdge)>> {
        nonempty(id)?;
        if cursor < 0 || limit == 0 {
            return Err(super::Error::InvalidInput(
                "edge page requires a nonnegative cursor and positive limit",
            ));
        }
        let endpoint = match direction {
            Direction::In => "target = ?1",
            Direction::Out => "source = ?1",
            Direction::Both => "(source = ?1 OR target = ?1)",
        };
        let mut sql = format!(
            "SELECT kind, source, target, line, col, provenance, metadata, id
             FROM edges WHERE {endpoint} AND status = 'resolved' AND id > ?2"
        );
        let mut values = vec![Value::Text(id.to_owned()), Value::Integer(cursor)];
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
            values.extend(names.into_iter().map(|name| Value::Text(name.to_owned())));
        }
        sql.push_str(" ORDER BY id LIMIT ?");
        values.push(Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
        let mut statement = self.connection.prepare(&sql)?;
        Ok(statement
            .query_map(params_from_iter(values), |row| {
                Ok((row.get(7)?, edge_from_row(row)?))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    #[cfg(test)]
    pub(crate) fn neighborhood(
        &self,
        id: &str,
        direction: Direction,
        kinds: Option<&[GraphEdgeKind]>,
    ) -> Result<Vec<FileEdge>> {
        Ok(self
            .neighborhood_page(id, direction, kinds, 0, usize::MAX)?
            .into_iter()
            .map(|(_, edge)| edge)
            .collect())
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
