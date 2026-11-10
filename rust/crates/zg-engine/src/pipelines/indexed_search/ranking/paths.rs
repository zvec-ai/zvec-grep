//! Conservative path priors over workspace-relative components.
use super::query::QueryFeatures;
use crate::api::context::options::QueryFilter;

#[derive(Clone, Copy, Debug, Default)]
// These independent features may all apply at once.
#[allow(clippy::struct_excessive_bools)]
pub(super) struct Roles {
    pub(super) tests: bool,
    pub(super) dependency: bool,
    pub(super) example: bool,
    pub(super) generated: bool,
}

impl Roles {
    pub(super) fn from_path(path: &str) -> Self {
        let lower = path.replace('\\', "/").to_lowercase();
        let parts = lower
            .split('/')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        let file = parts.last().copied().unwrap_or_default();
        let directories = parts
            .get(..parts.len().saturating_sub(1))
            .unwrap_or_default();
        let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
        Self {
            tests: directories
                .iter()
                .any(|s| matches!(*s, "test" | "tests" | "__tests__" | "spec" | "specs"))
                || file.contains(".test.")
                || file.contains(".spec.")
                || stem.ends_with("_test")
                || file.starts_with("test_")
                || stem == "test",
            dependency: directories.iter().any(|s| {
                matches!(
                    *s,
                    "vendor" | "thirdparty" | "third_party" | "third-party" | "node_modules"
                )
            }),
            example: directories.iter().any(|s| {
                matches!(
                    *s,
                    "example" | "examples" | "fixtures" | "__fixtures__" | "mocks" | "__mocks__"
                )
            }),
            // Do not infer generated code from dist/, build/, or .d.ts alone.
            generated: file.contains(".generated.")
                || file.ends_with(".pb.go")
                || file.ends_with(".g.cs"),
        }
    }

    pub(super) fn overrides(query: &QueryFeatures, filter: &QueryFilter) -> Self {
        let included = filter
            .globs
            .iter()
            .filter(|r| !r.pattern.starts_with('!'))
            .map(|r| Self::from_path(&r.pattern))
            .collect::<Vec<_>>();
        let explicit = query
            .paths
            .iter()
            .map(|path| Self::from_path(path))
            .collect::<Vec<_>>();
        Self {
            tests: explicit.iter().any(|r| r.tests)
                || query.tests
                || (!included.is_empty() && included.iter().all(|r| r.tests)),
            dependency: explicit.iter().any(|r| r.dependency)
                || query.dependencies
                || (!included.is_empty() && included.iter().all(|r| r.dependency)),
            example: explicit.iter().any(|r| r.example)
                || query.examples
                || (!included.is_empty() && included.iter().all(|r| r.example)),
            generated: explicit.iter().any(|r| r.generated)
                || query.generated
                || (!included.is_empty() && included.iter().all(|r| r.generated)),
        }
    }

    pub(super) fn factor(self, overrides: Self) -> f64 {
        [
            (self.tests && !overrides.tests, 0.65_f64),
            (self.dependency && !overrides.dependency, 0.60),
            (self.example && !overrides.example, 0.75),
            (self.generated && !overrides.generated, 0.55),
        ]
        .into_iter()
        .filter(|(active, _)| *active)
        .map(|(_, factor)| factor)
        .fold(1.0, f64::min)
    }

    pub(super) fn names(self) -> Vec<String> {
        [
            (self.tests, "test"),
            (self.dependency, "dependency"),
            (self.example, "example"),
            (self.generated, "generated"),
        ]
        .into_iter()
        .filter(|(active, _)| *active)
        .map(|(_, name)| name.to_owned())
        .collect()
    }
}
