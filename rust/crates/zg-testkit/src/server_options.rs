//! Node oracle results for server option selection.

use std::{collections::BTreeMap, path::Path};

use serde::Deserialize;

use crate::{FixtureError, fixture::CURRENT_FIXTURE_SCHEMA_VERSION};

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerOptionResult {
    Selected { value: String },
    Unset,
    ParseError { message: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerOptionCase {
    pub id: String,
    pub action: String,
    pub option: String,
    pub form: String,
    pub condition: String,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub node_result: ServerOptionResult,
    #[serde(default)]
    pub allowed_differences: Vec<String>,
    #[serde(default)]
    pub expected_rust: Option<ServerOptionResult>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerOptionCases {
    pub schema_version: u32,
    pub source_revision: String,
    pub cases: Vec<ServerOptionCase>,
}

/// Loads the server option oracle table.
///
/// # Errors
/// Returns an error for file, JSON, version or difference-field errors.
pub fn load_server_option_cases(path: &Path) -> Result<ServerOptionCases, FixtureError> {
    let display = path.display().to_string();
    let bytes = std::fs::read(path).map_err(|source| FixtureError::Read {
        path: display.clone(),
        source,
    })?;
    let table: ServerOptionCases =
        serde_json::from_slice(&bytes).map_err(|source| FixtureError::Decode {
            path: display.clone(),
            source,
        })?;
    if table.schema_version != CURRENT_FIXTURE_SCHEMA_VERSION {
        return Err(FixtureError::UnsupportedVersion {
            actual: table.schema_version,
            expected: CURRENT_FIXTURE_SCHEMA_VERSION,
        });
    }
    for case in &table.cases {
        let has_expected = case.expected_rust.is_some();
        if has_expected == case.allowed_differences.is_empty() {
            return Err(FixtureError::InvalidCase {
                path: display,
                case_id: case.id.clone(),
            });
        }
    }
    Ok(table)
}
