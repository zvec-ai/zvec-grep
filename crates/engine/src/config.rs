use crate::workspace::WorkspaceConfig;

/// Configuration used to construct an engine.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub workspace: WorkspaceConfig,
}

impl EngineConfig {
    pub fn new(workspace: WorkspaceConfig) -> Self {
        Self { workspace }
    }
}
