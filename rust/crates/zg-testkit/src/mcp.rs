//! Captured public MCP search responses from the Node.js implementation.

use std::path::Path;

use serde::Deserialize;

use crate::{FixtureError, fixture::CURRENT_FIXTURE_SCHEMA_VERSION};

#[derive(Debug, Deserialize)]
pub struct McpSearchPresentationCase {
    pub id: String,
    pub result: serde_json::Value,
    pub expected_short: String,
    pub expected_full: String,
}

#[derive(Deserialize)]
struct Cases {
    schema_version: u32,
    cases: Vec<McpSearchPresentationCase>,
}

/// Loads Node.js public MCP search presentation fixtures.
///
/// # Errors
/// Returns an error for unreadable files, invalid JSON or unsupported versions.
pub fn load_mcp_search_cases(path: &Path) -> Result<Vec<McpSearchPresentationCase>, FixtureError> {
    let bytes = std::fs::read(path).map_err(|source| FixtureError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let cases: Cases = serde_json::from_slice(&bytes).map_err(|source| FixtureError::Decode {
        path: path.display().to_string(),
        source,
    })?;
    if cases.schema_version != CURRENT_FIXTURE_SCHEMA_VERSION {
        return Err(FixtureError::UnsupportedVersion {
            actual: cases.schema_version,
            expected: CURRENT_FIXTURE_SCHEMA_VERSION,
        });
    }
    Ok(cases.cases)
}
