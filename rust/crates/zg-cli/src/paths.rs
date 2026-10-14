use std::path::Path;

/// Presents ordinary Windows paths without changing paths that require verbatim syntax.
#[must_use]
pub fn output_path(path: &Path) -> &Path {
    #[cfg(windows)]
    {
        dunce::simplified(path)
    }
    #[cfg(not(windows))]
    {
        path
    }
}
