use std::fmt;

use super::path::SourcePath;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct DirectoryId(u32);

impl DirectoryId {
    pub(crate) const fn new(value: u32) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for DirectoryId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A directory relative to the workspace root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryRecord {
    pub id: DirectoryId,
    pub relative_path: SourcePath,
}
