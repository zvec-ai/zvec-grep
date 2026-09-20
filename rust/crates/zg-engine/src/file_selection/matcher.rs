use std::path::Path;

use ignore::{
    Match,
    overrides::{Override, OverrideBuilder},
};

use crate::{EngineError, EngineResult, domain::GlobRule};

const MAX_GLOB_RULES: usize = 1_024;
const MAX_GLOB_BYTES: usize = 4_096;
const MAX_TOTAL_GLOB_BYTES: usize = 1_048_576;

/// Compiled ordered path globs; matching never reads disk.
#[derive(Clone, Debug)]
pub(crate) struct GlobMatcher {
    paths: Override,
}

impl GlobMatcher {
    pub(crate) fn new(root: &Path, rules: &[GlobRule]) -> EngineResult<Self> {
        if rules.len() > MAX_GLOB_RULES
            || rules.iter().map(|rule| rule.pattern.len()).sum::<usize>() > MAX_TOTAL_GLOB_BYTES
        {
            return Err(EngineError::invalid_argument(
                "glob rules exceed the rule count or total pattern size limit",
            ));
        }
        let mut builder = OverrideBuilder::new(root);
        for rule in rules {
            if rule.pattern.is_empty() || rule.pattern == "!" || rule.pattern.len() > MAX_GLOB_BYTES
            {
                return Err(EngineError::invalid_argument(format!(
                    "invalid glob {:?}: expected a non-empty pattern of at most {MAX_GLOB_BYTES} bytes",
                    rule.pattern
                )));
            }
            builder
                .case_insensitive(rule.case_insensitive)
                .map_err(|error| glob_error(&error))?;
            builder
                .add(&rule.pattern)
                .map_err(|error| glob_error(&error))?;
        }
        Ok(Self {
            paths: builder.build().map_err(|error| glob_error(&error))?,
        })
    }

    pub(crate) fn path_match(&self, path: &Path, is_directory: bool) -> Match<()> {
        self.paths.matched(path, is_directory).map(|_| ())
    }

    pub(crate) fn matches_path(&self, relative: &Path) -> bool {
        !self.path_match(relative, false).is_ignore()
            && relative
                .parent()
                .into_iter()
                .flat_map(Path::ancestors)
                .take_while(|parent| !parent.as_os_str().is_empty())
                .all(|parent| !self.path_match(parent, true).is_ignore())
    }
}

fn glob_error(error: &ignore::Error) -> EngineError {
    EngineError::invalid_argument(format!("invalid file glob: {error}"))
}
