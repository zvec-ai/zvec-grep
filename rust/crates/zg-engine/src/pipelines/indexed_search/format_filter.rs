//! Compile catalog format rules into predicates over stored basenames.

use std::{collections::BTreeMap, ffi::OsStr};

use crate::{
    api::context::options::QueryFilter, domain::FileFormat, storage::types::StoragePathFilter,
};

use super::path_filter::{all, any, negate};

pub(super) fn compile_format_filter(filter: &QueryFilter) -> Option<StoragePathFilter> {
    if filter.formats.is_empty()
        && filter.excluded_formats.is_empty()
        && filter.categories.is_empty()
        && filter.excluded_categories.is_empty()
    {
        return None;
    }
    let rules = CatalogRules::new();
    let mut predicates = Vec::new();
    if !filter.formats.is_empty() {
        predicates.push(rules.matching(&filter.formats));
    }
    if !filter.categories.is_empty() {
        let formats = FileFormat::ALL
            .iter()
            .copied()
            .filter(|format| {
                format
                    .categories()
                    .iter()
                    .any(|category| filter.categories.contains(category))
            })
            .collect::<Vec<_>>();
        predicates.push(rules.matching(&formats));
    }
    let excluded = FileFormat::ALL
        .iter()
        .copied()
        .filter(|format| {
            filter.excluded_formats.contains(format)
                || format
                    .categories()
                    .iter()
                    .any(|category| filter.excluded_categories.contains(category))
        })
        .collect::<Vec<_>>();
    if !excluded.is_empty() {
        predicates.push(negate(rules.matching(&excluded)));
    }
    Some(all(predicates))
}

struct CatalogRules {
    extensions: BTreeMap<&'static str, Vec<FileFormat>>,
    names: BTreeMap<&'static str, Vec<FileFormat>>,
}

impl CatalogRules {
    fn new() -> Self {
        let mut rules = Self {
            extensions: BTreeMap::new(),
            names: BTreeMap::new(),
        };
        for &format in FileFormat::ALL {
            for &extension in format.extensions() {
                rules.extensions.entry(extension).or_default().push(format);
            }
            for &name in format.file_names() {
                rules.names.entry(name).or_default().push(format);
            }
        }
        rules
    }

    fn matching(&self, selected: &[FileFormat]) -> StoragePathFilter {
        let extensions = self
            .extensions
            .iter()
            .map(|(&extension, formats)| (extension, selected_match(formats.iter(), selected)))
            .collect::<Vec<_>>();
        // Resolve the catalog's own literal names once when compiling the query.
        // Exact names add candidates; only specific formats suppress Text.
        let names = self
            .names
            .iter()
            .map(|(&name, formats)| {
                let suffix_formats = self
                    .extensions
                    .iter()
                    .filter(|(extension, _)| has_extension(name, extension))
                    .max_by_key(|(extension, _)| extension.len())
                    .map_or(&[][..], |(_, formats)| formats.as_slice());
                (
                    name,
                    selected_match(formats.iter().chain(suffix_formats), selected),
                )
            })
            .collect::<Vec<_>>();
        let mut predicates = Vec::new();
        for &(extension, matches) in &extensions {
            if !matches {
                continue;
            }
            let mut exceptions = extensions
                .iter()
                .filter(|(longer, matches)| !matches && has_extension(longer, extension))
                .map(|(longer, _)| extension_predicate(longer))
                .collect::<Vec<_>>();
            exceptions.extend(
                names
                    .iter()
                    .filter(|(name, matches)| !matches && has_extension(name, extension))
                    .map(|(name, _)| StoragePathFilter::FileNameExact((*name).to_owned())),
            );
            predicates.push(all(vec![
                extension_predicate(extension),
                negate(any(exceptions)),
            ]));
        }
        predicates.extend(
            names
                .iter()
                .filter(|(_, matches)| *matches)
                .map(|(name, _)| StoragePathFilter::FileNameExact((*name).to_owned())),
        );
        if selected.contains(&FileFormat::Unknown) {
            let known = any(self
                .extensions
                .keys()
                .map(|extension| extension_predicate(extension))
                .chain(
                    self.names
                        .keys()
                        .map(|name| StoragePathFilter::FileNameExact((*name).to_owned())),
                )
                .collect());
            predicates.push(negate(known));
        }
        any(predicates)
    }
}

fn selected_match<'a>(
    formats: impl Iterator<Item = &'a FileFormat> + Clone,
    selected: &[FileFormat],
) -> bool {
    let specific = formats
        .clone()
        .any(|format| !matches!(format, FileFormat::Text | FileFormat::Unknown));
    formats
        .filter(|format| !specific || **format != FileFormat::Text)
        .any(|format| selected.contains(format))
}

fn has_extension(name: &str, extension: &str) -> bool {
    name.strip_suffix(extension)
        .and_then(|prefix| prefix.strip_suffix('.'))
        .is_some_and(|stem| !stem.is_empty())
}

fn extension_predicate(extension: &str) -> StoragePathFilter {
    let suffix = format!(".{extension}");
    // A leading dot alone does not introduce an extension: `.rs` is not Rust.
    all(vec![
        StoragePathFilter::FileNameSuffix(suffix.clone()),
        negate(StoragePathFilter::FileNameExact(suffix)),
    ])
}

/// Fallback for backends without path predicates or native non-Unicode basenames.
/// Evaluate the compiled rules directly without constructing per-file formats.
pub(super) fn matches_file_name(predicate: &StoragePathFilter, name: &OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    match predicate {
        StoragePathFilter::All => true,
        StoragePathFilter::None => false,
        StoragePathFilter::FileNameExact(value) => bytes == value.as_bytes(),
        StoragePathFilter::FileNamePrefix(value) => bytes.starts_with(value.as_bytes()),
        StoragePathFilter::FileNameSuffix(value) => bytes.ends_with(value.as_bytes()),
        StoragePathFilter::And(predicates) => predicates.iter().all(|p| matches_file_name(p, name)),
        StoragePathFilter::Or(predicates) => predicates.iter().any(|p| matches_file_name(p, name)),
        StoragePathFilter::Not(predicate) => !matches_file_name(predicate, name),
        StoragePathFilter::Directory(_) => {
            unreachable!("format rules only reference basenames")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{compile_format_filter, matches_file_name};
    use crate::{
        api::context::options::QueryFilter,
        domain::{FileCategory, FileFormat},
    };

    fn matches(filter: &QueryFilter, name: &str) -> bool {
        matches_file_name(
            &compile_format_filter(filter).expect("format condition"),
            name.as_ref(),
        )
    }

    #[test]
    fn catalog_spellings_and_longest_extensions_compile_without_source_detection() {
        for (format, names, expected) in [
            (
                FileFormat::Rust,
                vec!["main.rs", "MAIN.RS", ".rs", ".hidden.rs"],
                vec![true, false, false, true],
            ),
            (
                FileFormat::Jpeg,
                vec![
                    "photo.jpg",
                    "photo.JPEG",
                    "photo.Jpg",
                    "photo.Jpeg",
                    "photo.jPg",
                    ".jpeg",
                ],
                vec![true, true, true, true, false, false],
            ),
            (
                FileFormat::Cpp,
                vec!["main.C", "header.H", "header.h"],
                vec![true, true, true],
            ),
            (
                FileFormat::C,
                vec!["main.c", "main.C", "header.h"],
                vec![true, false, true],
            ),
            (
                FileFormat::Tar,
                vec!["archive.tar.gz", ".tar.gz", "archive.gz"],
                vec![true, false, false],
            ),
            (
                FileFormat::Gzip,
                vec!["archive.tar.gz", ".tar.gz", "archive.gz"],
                vec![false, true, true],
            ),
            (
                FileFormat::Mpeg,
                vec!["module.ts", "module.d.ts", ".d.ts"],
                vec![true, false, true],
            ),
            (
                FileFormat::Text,
                vec!["plain.txt", "CMakeLists.txt", "CMakeCache.txt"],
                vec![true, false, false],
            ),
            (
                FileFormat::Unknown,
                vec!["script", "main.RS", ".rs", "font.otf"],
                vec![true, true, true, false],
            ),
        ] {
            let filter = QueryFilter {
                formats: vec![format],
                ..Default::default()
            };
            for (name, expected) in names.into_iter().zip(expected) {
                assert_eq!(matches(&filter, name), expected, "{format:?}: {name}");
            }
        }
    }

    #[test]
    fn multiple_formats_categories_and_exclusions_keep_their_independent_meanings() {
        let mut filter = QueryFilter {
            formats: vec![FileFormat::Json],
            categories: vec![FileCategory::Code],
            ..Default::default()
        };
        assert!(matches(&filter, "tsconfig.json"));
        assert!(!matches(&filter, "settings.json"));
        filter.excluded_formats.push(FileFormat::TypeScript);
        assert!(!matches(&filter, "tsconfig.json"));

        filter = QueryFilter {
            formats: vec![FileFormat::Cpp],
            excluded_formats: vec![FileFormat::C],
            ..Default::default()
        };
        assert!(!matches(&filter, "header.h"));
        assert!(matches(&filter, "header.hpp"));
        filter = QueryFilter {
            categories: vec![FileCategory::Code],
            excluded_categories: vec![FileCategory::Document],
            ..Default::default()
        };
        assert!(!matches(&filter, "page.html"));
        assert!(matches(&filter, "main.rs"));
        filter = QueryFilter {
            categories: vec![FileCategory::Unknown],
            ..Default::default()
        };
        assert!(matches(&filter, "font.otf"));
        assert!(matches(&filter, "script"));
        assert!(!matches(&filter, "main.rs"));
        filter = QueryFilter {
            formats: vec![FileFormat::Gzip, FileFormat::Tar],
            ..Default::default()
        };
        assert!(matches(&filter, "archive.tar.gz"));
        assert!(matches(&filter, ".tar.gz"));
        filter = QueryFilter {
            formats: vec![FileFormat::Text, FileFormat::Cmake],
            ..Default::default()
        };
        assert!(matches(&filter, "CMakeLists.txt"));
        assert!(matches(&filter, "notes.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn fallback_matches_registered_suffixes_on_native_names() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
        let predicate = compile_format_filter(&QueryFilter {
            formats: vec![FileFormat::Rust],
            ..Default::default()
        })
        .expect("filter");
        assert!(matches_file_name(&predicate, OsStr::from_bytes(b"\xff.rs")));
        assert!(!matches_file_name(
            &predicate,
            OsStr::from_bytes(b"\xff.RS")
        ));
    }
}
