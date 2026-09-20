use std::{collections::BTreeMap, path::Path};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CURRENT_FIXTURE_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CliCompatibilityCase {
    pub schema_version: u32,
    pub id: String,
    pub description: String,
    pub input: CliFixtureInput,
    pub expected: CliFixtureExpected,
    #[serde(default)]
    pub allowed_differences: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CliFixtureInput {
    pub argv: Vec<String>,
    pub cwd_fixture: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CliFixtureExpected {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Error)]
pub enum FixtureError {
    #[error("failed to read fixture {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to decode fixture {path}: {source}")]
    Decode {
        path: String,
        source: serde_json::Error,
    },
    #[error("unsupported fixture schema version {actual}; expected {expected}")]
    UnsupportedVersion { actual: u32, expected: u32 },
}

/// Loads and validates one CLI compatibility fixture.
///
/// # Errors
///
/// Returns [`FixtureError`] for I/O, JSON or schema-version failures.
pub fn load_cli_case(path: &Path) -> Result<CliCompatibilityCase, FixtureError> {
    let display = path.display().to_string();
    let bytes = std::fs::read(path).map_err(|source| FixtureError::Read {
        path: display.clone(),
        source,
    })?;
    let fixture: CliCompatibilityCase =
        serde_json::from_slice(&bytes).map_err(|source| FixtureError::Decode {
            path: display,
            source,
        })?;
    if fixture.schema_version != CURRENT_FIXTURE_SCHEMA_VERSION {
        return Err(FixtureError::UnsupportedVersion {
            actual: fixture.schema_version,
            expected: CURRENT_FIXTURE_SCHEMA_VERSION,
        });
    }
    Ok(fixture)
}
