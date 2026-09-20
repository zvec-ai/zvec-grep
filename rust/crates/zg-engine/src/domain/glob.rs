use serde::{Deserialize, Serialize};

/// A path glob using ripgrep's command-line glob semantics.
/// Paths are matched relative to a caller-supplied root.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GlobRule {
    /// A leading `!` excludes matching paths.
    pub pattern: String,
    #[serde(default)]
    pub case_insensitive: bool,
}

impl From<String> for GlobRule {
    fn from(pattern: String) -> Self {
        Self {
            pattern,
            case_insensitive: false,
        }
    }
}

impl From<&str> for GlobRule {
    fn from(pattern: &str) -> Self {
        pattern.to_owned().into()
    }
}
