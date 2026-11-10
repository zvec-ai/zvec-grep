//! Compatibility fixture readers shared by workspace tests.

mod fixture;
mod mcp;
mod server_options;

pub use fixture::{CliCompatibilityCase, FixtureError, load_cli_case};
pub use mcp::{McpSearchPresentationCase, load_mcp_search_cases};
pub use server_options::{
    ServerOptionCase, ServerOptionCases, ServerOptionResult, load_server_option_cases,
};
