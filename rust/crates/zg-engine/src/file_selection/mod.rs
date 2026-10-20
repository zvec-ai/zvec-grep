//! Engine-owned file matching and filesystem selection policy.

mod matcher;
mod policy;
mod portable_paths;

pub(crate) use matcher::GlobMatcher;
pub(crate) use policy::{ScanPolicy, is_within_index};
pub(crate) use portable_paths::verify_source_paths;

#[cfg(test)]
mod tests;

/// Use the same catalog as the embedded ripgrep backend, not extractor names.
pub(crate) fn file_types(
    included: &[String],
    excluded: &[String],
) -> crate::EngineResult<ignore::types::Types> {
    let mut builder = ignore::types::TypesBuilder::new();
    builder.add_defaults();
    for name in included {
        builder.select(name);
    }
    for name in excluded {
        builder.negate(name);
    }
    builder
        .build()
        .map_err(|error| crate::EngineError::invalid_argument(error.to_string()))
}
