//! Compatibility fixture readers shared by workspace tests.

mod fixture;
mod mcp;

pub use fixture::{CliCompatibilityCase, FixtureError, load_cli_case};
pub use mcp::{McpSearchPresentationCase, load_mcp_search_cases};
