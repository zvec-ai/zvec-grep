//! Engine-owned file matching and filesystem selection policy.

mod matcher;
mod policy;

pub(crate) use matcher::GlobMatcher;
pub(crate) use policy::ScanPolicy;

#[cfg(test)]
mod tests;
