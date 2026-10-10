//! SQLite persistence for directed code relationships and pending references.
//!
//! Nodes and file metadata remain in the entity store. Each file replacement is
//! atomic within SQLite; the indexing coordinator owns consistency with zvec.
//! Callers must serialize reference reads, resolution and writeback with file updates.
//! This module does not resolve names, run FTS, or manage workspace locks.
//! Indexing integration is pending; this storage is not yet opened by `IndexStore`.

mod pending;
mod reader;
mod schema;
mod types;
mod writer;

use rusqlite::{Connection, OpenFlags};
use std::{path::Path, time::Duration};
pub(crate) use types::*;

/// Errors at the graph persistence boundary.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("SQLite graph operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("invalid graph metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("graph filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid graph input: {0}")]
    InvalidInput(&'static str),
    #[error(
        "unsupported graph database (version {version}, application {application_id}); rebuild the index"
    )]
    UnsupportedSchema { version: i64, application_id: i64 },
    #[error("refusing to initialize an unrelated SQLite database")]
    ForeignDatabase,
}

/// Result of a graph storage operation.
pub(crate) type Result<T> = std::result::Result<T, Error>;

/// Read-only opening never creates a database or modifies its schema.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OpenMode {
    ReadOnly,
    ReadWrite,
}

/// Owns one connection. Writes require exclusive Rust access; readers and writers
/// in separate connections use SQLite transactions and a five-second busy timeout.
/// Drop closes the connection; `close` additionally reports close errors.
pub(crate) struct SqliteGraphStorage {
    connection: Connection,
}

impl SqliteGraphStorage {
    /// Opens an existing reader, or initializes a writer and its parent directory.
    ///
    /// # Errors
    /// Rejects missing read-only databases, foreign/newer schemas, and SQLite/I/O errors.
    pub(crate) fn open(path: &Path, mode: OpenMode) -> Result<Self> {
        if path.as_os_str().is_empty() {
            return Err(Error::InvalidInput("database path is empty"));
        }
        let flags = match mode {
            OpenMode::ReadOnly => OpenFlags::SQLITE_OPEN_READ_ONLY,
            OpenMode::ReadWrite => {
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent)?;
                }
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
            }
        };
        Self::configure(Connection::open_with_flags(path, flags)?, mode)
    }

    /// Creates an isolated writable graph, primarily for tests.
    ///
    /// # Errors
    /// Returns SQLite initialization errors.
    pub(crate) fn in_memory() -> Result<Self> {
        Self::configure(Connection::open_in_memory()?, OpenMode::ReadWrite)
    }

    fn configure(mut connection: Connection, mode: OpenMode) -> Result<Self> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        match mode {
            OpenMode::ReadOnly => schema::validate(&connection)?,
            OpenMode::ReadWrite => {
                schema::initialize(&mut connection)?;
                connection.pragma_update(None, "journal_mode", "WAL")?;
            }
        }
        Ok(Self { connection })
    }

    /// Closes the connection. The value cannot be used after closing.
    ///
    /// # Errors
    /// Reports SQLite close failures.
    pub(crate) fn close(self) -> Result<()> {
        self.connection.close().map_err(|(_, error)| error.into())
    }
}

fn nonempty(value: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(Error::InvalidInput("ID or reference name is empty"))
    } else {
        Ok(())
    }
}

fn decode_enum<T: serde::de::DeserializeOwned>(value: String) -> rusqlite::Result<T> {
    serde_json::from_value(serde_json::Value::String(value)).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn decode_metadata(value: &str) -> rusqlite::Result<Metadata> {
    serde_json::from_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

#[cfg(test)]
mod tests;
