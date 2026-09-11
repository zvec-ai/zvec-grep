use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
    sync::{Arc, Mutex, OnceLock},
    time::SystemTime,
};

use crate::HostError;
use ignore::types::TypesBuilder;

use crate::{
    DiscoveryOptions, RootSpec,
    pattern::{PathPattern, is_hidden_name, normalize_path_pattern, normalize_relative_path},
};

const HARD_SKIP_HIDDEN_NAMES: [&str; 2] = [".git", ".zvec-grep"];

const DEFAULT_IGNORED_DIRECTORY_NAMES: [&str; 36] = [
    "node_modules",
    "vendor",
    "thirdparty",
    "third_party",
    "external",
    "deps",
    "dist",
    "build",
    "out",
    "target",
    "coverage",
    "generated",
    "__pycache__",
    "venv",
    ".venv",
    "env",
    ".tox",
    ".eggs",
    "Pods",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".vite",
    ".parcel-cache",
    ".cache",
    ".gradle",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    "tmp",
    "temp",
    "logs",
    "locale",
    "locales",
    "translations",
];

const DEFAULT_IGNORED_FILE_PATTERNS: [&str; 23] = [
    "*.lock",
    "*.lockb",
    "*-lock.json",
    "*-lock.yaml",
    "npm-shrinkwrap.json",
    "go.sum",
    "*.resolved",
    "*.po",
    "*.pot",
    "*.map",
    "*.min.*",
    "*.bundle.*",
    "*.generated.*",
    "*.gen.*",
    "*.designer.*",
    "*.pb.*",
    "*_pb2.*",
    "*.g.*",
    "*.gif",
    "*.jpeg",
    "*.jpg",
    "*.png",
    "*.webp",
];

const RIPGREP_FILE_TYPE_ALIASES: [(&str, &str); 24] = [
    ("bash", "sh"),
    ("cjs", "js"),
    ("cp", "cpp"),
    ("cc", "cpp"),
    ("cpp", "cpp"),
    ("cxx", "cpp"),
    ("hpp", "h"),
    ("hxx", "h"),
    ("hh", "h"),
    ("h", "h"),
    ("js", "js"),
    ("jsx", "js"),
    ("mjs", "js"),
    ("markdown", "md"),
    ("mdx", "md"),
    ("pyi", "py"),
    ("py", "py"),
    ("rb", "ruby"),
    ("rs", "rust"),
    ("ts", "ts"),
    ("tsx", "ts"),
    ("yml", "yaml"),
    ("zsh", "sh"),
    ("sh", "sh"),
];

type FileTypeMap = HashMap<String, Vec<String>>;
type FileTypeCache = OnceLock<FileTypeMap>;

#[derive(Clone, Debug)]
pub(crate) struct FileTypeResolver {
    cache: Arc<FileTypeCache>,
}

impl FileTypeResolver {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(OnceLock::new()),
        }
    }

    fn resolve(
        &self,
        included: &[String],
        excluded: &[String],
    ) -> Result<FileTypePatterns, HostError> {
        if included.is_empty() && excluded.is_empty() {
            return Ok(FileTypePatterns::default());
        }
        let type_map = self.cache.get_or_init(load_default_type_map);
        Ok(FileTypePatterns {
            include: resolve_type_names(included, type_map)?,
            exclude: resolve_type_names(excluded, type_map)?,
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RootPolicy {
    root: RootSpec,
    include: Vec<PathPattern>,
    exclude: Vec<PathPattern>,
    ordered_globs: Vec<OrderedGlob>,
    file_types: FileTypePatterns,
    base_ignore_rules: Vec<IgnoreRule>,
    gitignore_cache: Arc<Mutex<HashMap<std::path::PathBuf, GitignoreCacheEntry>>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PathInterest {
    file: bool,
    directory: bool,
}

impl PathInterest {
    pub fn is_empty(self) -> bool {
        !self.file && !self.directory
    }

    fn allows(self, is_directory: bool) -> bool {
        if is_directory {
            self.directory
        } else {
            self.file
        }
    }
}

#[derive(Clone, Debug)]
struct GitignoreCacheEntry {
    fingerprint: Option<(u64, SystemTime)>,
    rules: Vec<IgnoreRule>,
}

impl RootPolicy {
    pub fn new(root: RootSpec, resolver: &FileTypeResolver) -> Result<Self, HostError> {
        let root = normalize_root(root)?;
        let discovery = &root.discovery;
        let include = compile_path_patterns(&discovery.include_paths)?;
        let exclude = compile_path_patterns(&discovery.exclude_paths)?;
        let ordered_globs = compile_ordered_globs(discovery)?;
        let file_types = resolver.resolve(&discovery.file_types, &discovery.excluded_file_types)?;
        let mut base_ignore_rules = if discovery.no_ignore {
            Vec::new()
        } else {
            default_ignore_rules()?
        };
        base_ignore_rules.extend(read_configured_ignore_rules(&root)?);
        Ok(Self {
            root,
            include,
            exclude,
            ordered_globs,
            file_types,
            base_ignore_rules,
            gitignore_cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn root(&self) -> &RootSpec {
        &self.root
    }

    pub fn root_path(&self) -> &Path {
        &self.root.path
    }

    pub fn initial_ignore_rules(&self) -> Vec<IgnoreRule> {
        self.base_ignore_rules.clone()
    }

    pub fn rules_with_gitignore(&self, parent: &[IgnoreRule], directory: &Path) -> Vec<IgnoreRule> {
        let mut rules = parent.to_vec();
        if !self.root.discovery.no_ignore {
            rules.extend(self.read_gitignore_rules(directory));
        }
        rules
    }

    pub fn rules_for_directory(&self, directory: &Path) -> Vec<IgnoreRule> {
        let Ok(path_from_root) = directory.strip_prefix(&self.root.path) else {
            return self.base_ignore_rules.clone();
        };
        let mut rules = self.base_ignore_rules.clone();
        let mut current = self.root.path.clone();
        if !self.root.discovery.no_ignore {
            rules.extend(self.read_gitignore_rules(&current));
        }
        for segment in path_from_root.components() {
            current.push(segment.as_os_str());
            if !self.root.discovery.no_ignore {
                rules.extend(self.read_gitignore_rules(&current));
            }
        }
        rules
    }

    pub fn path_can_be_scanned(
        &self,
        relative_path: &str,
        name: &str,
        is_directory: bool,
        ignore_rules: &[IgnoreRule],
    ) -> bool {
        if relative_path
            .split('/')
            .any(|segment| HARD_SKIP_HIDDEN_NAMES.contains(&segment))
        {
            return false;
        }
        let ignore_match = match_ignore_rules(relative_path, is_directory, ignore_rules);
        if ignore_match.ignored
            && !self.ignored_path_explicitly_included(relative_path, &ignore_match)
        {
            return false;
        }
        if self
            .exclude
            .iter()
            .any(|pattern| pattern.is_match(relative_path))
        {
            return false;
        }
        if is_directory {
            return !self.should_skip_hidden_directory(name, relative_path);
        }
        !self.should_skip_hidden_file(name, relative_path)
            && (self.include.is_empty()
                || self
                    .include
                    .iter()
                    .any(|pattern| pattern.is_match(relative_path)))
            && self.matches_file_selection(relative_path)
    }

    pub fn path_can_affect_index(&self, absolute_path: &Path, is_directory: bool) -> bool {
        self.path_interest_can_affect_index(
            absolute_path,
            is_directory,
            self.classify_path_interest(absolute_path),
        )
    }

    /// Evaluates discovery and ignore rules once before a watcher pays for
    /// target metadata. Both shapes are retained when metadata is required.
    pub fn classify_path_interest(&self, absolute_path: &Path) -> PathInterest {
        let Ok(path_from_root) = absolute_path.strip_prefix(&self.root.path) else {
            return PathInterest::default();
        };
        if path_from_root.as_os_str().is_empty() {
            return PathInterest {
                file: true,
                directory: true,
            };
        }
        let depth = path_from_root.components().count();
        let file_shape_allowed = (self.root.recursive
            || absolute_path.parent() == Some(self.root.path.as_path()))
            && self
                .root
                .discovery
                .max_depth
                .is_none_or(|maximum| depth <= maximum);
        let directory_shape_allowed = self.root.recursive
            && self
                .root
                .discovery
                .max_depth
                .is_none_or(|maximum| depth < maximum);
        if !file_shape_allowed && !directory_shape_allowed {
            return PathInterest::default();
        }
        let relative_path = normalize_relative_path(path_from_root);
        let name = absolute_path
            .file_name()
            .map_or_else(String::new, |value| value.to_string_lossy().into_owned());
        let rules = self.rules_for_directory(absolute_path.parent().unwrap_or(&self.root.path));
        PathInterest {
            file: file_shape_allowed
                && self.path_can_be_scanned(&relative_path, &name, false, &rules),
            directory: directory_shape_allowed
                && self.path_can_be_scanned(&relative_path, &name, true, &rules),
        }
    }

    pub fn path_interest_can_affect_index(
        &self,
        absolute_path: &Path,
        is_directory: bool,
        interest: PathInterest,
    ) -> bool {
        interest.allows(is_directory)
            && !self.has_excluded_nested_git_ancestor(absolute_path, is_directory)
    }

    pub fn invalidate_gitignore_rules(&self, directory: &Path) {
        lock(&self.gitignore_cache).remove(directory);
    }

    pub fn matches_file_selection(&self, relative_path: &str) -> bool {
        let has_positive = self.ordered_globs.iter().any(|rule| !rule.negated);
        let mut included = !has_positive;
        for rule in &self.ordered_globs {
            if rule.pattern.is_match(relative_path) {
                included = !rule.negated;
            }
        }
        included
            && (self.file_types.include.is_empty()
                || self
                    .file_types
                    .include
                    .iter()
                    .any(|pattern| pattern.is_match(relative_path)))
            && !self
                .file_types
                .exclude
                .iter()
                .any(|pattern| pattern.is_match(relative_path))
    }

    pub fn nested_git_repository_explicitly_included(&self, relative_path: &str) -> bool {
        self.include.iter().any(|pattern| {
            pattern.is_match(relative_path)
                || pattern
                    .normalized()
                    .starts_with(&format!("{relative_path}/"))
        })
    }

    pub fn is_hard_skipped_name(name: &str) -> bool {
        HARD_SKIP_HIDDEN_NAMES.contains(&name)
    }

    fn read_gitignore_rules(&self, directory: &Path) -> Vec<IgnoreRule> {
        let path = directory.join(".gitignore");
        let fingerprint = fs::metadata(&path)
            .ok()
            .and_then(|metadata| Some((metadata.len(), metadata.modified().ok()?)));
        if let Some(cached) = lock(&self.gitignore_cache).get(directory)
            && cached.fingerprint == fingerprint
        {
            return cached.rules.clone();
        }
        let rules = fs::read(path).map_or_else(
            |_| Vec::new(),
            |content| {
                let base_path = directory
                    .strip_prefix(&self.root.path)
                    .map_or_else(|_| String::new(), normalize_relative_path);
                parse_gitignore_rules(&String::from_utf8_lossy(&content), &base_path)
                    .unwrap_or_default()
            },
        );
        lock(&self.gitignore_cache).insert(
            directory.to_path_buf(),
            GitignoreCacheEntry {
                fingerprint,
                rules: rules.clone(),
            },
        );
        rules
    }

    fn ignored_path_explicitly_included(
        &self,
        relative_path: &str,
        ignore_match: &IgnoreMatch,
    ) -> bool {
        let Some(rule) = ignore_match.matched_rule.as_ref() else {
            return false;
        };
        self.include.iter().any(|include| {
            include_pattern_names_ignored_path(include, relative_path, rule.pattern.normalized())
        })
    }

    fn should_skip_hidden_directory(&self, name: &str, relative_path: &str) -> bool {
        is_hidden_name(name)
            && !self.root.discovery.hidden
            && !self.include.iter().any(|pattern| {
                include_pattern_declares_hidden_directory(pattern, relative_path)
                    && pattern.might_match_descendant(relative_path)
            })
    }

    fn should_skip_hidden_file(&self, name: &str, relative_path: &str) -> bool {
        is_hidden_name(name)
            && !self.root.discovery.hidden
            && !self.include.iter().any(|pattern| {
                include_pattern_declares_hidden_directory(pattern, relative_path)
                    && pattern.is_match(relative_path)
            })
    }

    fn has_excluded_nested_git_ancestor(&self, absolute_path: &Path, include_target: bool) -> bool {
        let Ok(relative) = absolute_path.strip_prefix(&self.root.path) else {
            return false;
        };
        let mut segments: Vec<_> = relative.components().collect();
        if !include_target {
            segments.pop();
        }
        let mut current = self.root.path.clone();
        for segment in segments {
            current.push(segment.as_os_str());
            let relative_directory = current
                .strip_prefix(&self.root.path)
                .map_or_else(|_| String::new(), normalize_relative_path);
            if is_nested_git_repository_directory(&current)
                && !self.nested_git_repository_explicitly_included(&relative_directory)
            {
                return true;
            }
        }
        false
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, Debug)]
pub(crate) struct IgnoreRule {
    base_path: String,
    pattern: PathPattern,
    flags: IgnoreRuleFlags,
}

#[derive(Clone, Copy, Debug)]
struct IgnoreRuleFlags(u8);

impl IgnoreRuleFlags {
    const NEGATED: u8 = 1;
    const DIRECTORY_ONLY: u8 = 1 << 1;
    const ANCHORED: u8 = 1 << 2;
    const HAS_SLASH: u8 = 1 << 3;

    fn new(negated: bool, directory_only: bool, anchored: bool, pattern: &str) -> Self {
        let mut flags = 0;
        if negated {
            flags |= Self::NEGATED;
        }
        if directory_only {
            flags |= Self::DIRECTORY_ONLY;
        }
        if anchored {
            flags |= Self::ANCHORED;
        }
        if pattern.contains('/') {
            flags |= Self::HAS_SLASH;
        }
        Self(flags)
    }

    fn contains(self, flag: u8) -> bool {
        self.0 & flag != 0
    }
}

#[derive(Clone, Debug, Default)]
struct IgnoreMatch {
    ignored: bool,
    matched_rule: Option<IgnoreRule>,
}

#[derive(Clone, Debug)]
struct OrderedGlob {
    pattern: PathPattern,
    negated: bool,
}

#[derive(Clone, Debug, Default)]
struct FileTypePatterns {
    include: Vec<PathPattern>,
    exclude: Vec<PathPattern>,
}

fn normalize_root(mut root: RootSpec) -> Result<RootSpec, HostError> {
    root.path = std::path::absolute(&root.path).map_err(|error| {
        HostError::storage_failure(
            "native-scanner",
            format!("could not resolve root {}: {error}", root.path.display()),
        )
    })?;
    Ok(root)
}

fn compile_path_patterns(patterns: &[String]) -> Result<Vec<PathPattern>, HostError> {
    patterns
        .iter()
        .filter(|pattern| !pattern.trim().is_empty())
        .map(|pattern| PathPattern::path(pattern))
        .collect()
}

fn compile_ordered_globs(discovery: &DiscoveryOptions) -> Result<Vec<OrderedGlob>, HostError> {
    discovery
        .globs
        .iter()
        .map(|pattern| (pattern, false))
        .chain(
            discovery
                .insensitive_globs
                .iter()
                .map(|pattern| (pattern, true)),
        )
        .filter_map(|(raw, case_insensitive)| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return None;
            }
            let (negated, pattern) = trimmed
                .strip_prefix('!')
                .map_or((false, trimmed), |value| (true, value.trim()));
            (!pattern.is_empty()).then_some((negated, pattern, case_insensitive))
        })
        .map(|(negated, pattern, case_insensitive)| {
            Ok(OrderedGlob {
                pattern: PathPattern::rg(pattern, case_insensitive)?,
                negated,
            })
        })
        .collect()
}

fn default_ignore_rules() -> Result<Vec<IgnoreRule>, HostError> {
    DEFAULT_IGNORED_DIRECTORY_NAMES
        .iter()
        .map(|pattern| IgnoreRule::new("", pattern, false, true, false))
        .chain(
            DEFAULT_IGNORED_FILE_PATTERNS
                .iter()
                .map(|pattern| IgnoreRule::new("", pattern, false, false, false)),
        )
        .collect()
}

fn read_configured_ignore_rules(root: &RootSpec) -> Result<Vec<IgnoreRule>, HostError> {
    let mut rules = Vec::new();
    for path in &root.discovery.ignore_files {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            root.path.join(path)
        };
        let content = fs::read(&absolute).map_err(|error| {
            HostError::storage_failure(
                "native-scanner",
                format!("could not read ignore file {}: {error}", absolute.display()),
            )
        })?;
        rules.extend(parse_gitignore_rules(
            &String::from_utf8_lossy(&content),
            "",
        )?);
    }
    Ok(rules)
}

fn parse_gitignore_rules(content: &str, base_path: &str) -> Result<Vec<IgnoreRule>, HostError> {
    content
        .lines()
        .filter_map(|line| parse_gitignore_rule(line, base_path).transpose())
        .collect()
}

fn parse_gitignore_rule(line: &str, base_path: &str) -> Result<Option<IgnoreRule>, HostError> {
    let mut pattern = line.trim();
    if pattern.is_empty() || pattern.starts_with('#') {
        return Ok(None);
    }
    let mut negated = false;
    if pattern.starts_with("\\#") || pattern.starts_with("\\!") {
        pattern = &pattern[1..];
    } else if let Some(value) = pattern.strip_prefix('!') {
        negated = true;
        pattern = value.trim();
    }
    if pattern.is_empty() {
        return Ok(None);
    }
    let directory_only = pattern.ends_with('/');
    let pattern = pattern.trim_end_matches('/');
    let anchored = pattern.starts_with('/');
    let pattern = pattern.trim_start_matches('/');
    let normalized = normalize_path_pattern(pattern);
    if normalized.is_empty() {
        return Ok(None);
    }
    Ok(Some(IgnoreRule::new(
        base_path,
        &normalized,
        negated,
        directory_only,
        anchored,
    )?))
}

impl IgnoreRule {
    fn new(
        base_path: &str,
        pattern: &str,
        negated: bool,
        directory_only: bool,
        anchored: bool,
    ) -> Result<Self, HostError> {
        Ok(Self {
            base_path: base_path.to_owned(),
            pattern: PathPattern::path(pattern)?,
            flags: IgnoreRuleFlags::new(negated, directory_only, anchored, pattern),
        })
    }
}

fn match_ignore_rules(
    relative_path: &str,
    is_directory: bool,
    rules: &[IgnoreRule],
) -> IgnoreMatch {
    let mut result = IgnoreMatch::default();
    for rule in rules {
        if ignore_rule_matches(rule, relative_path, is_directory) {
            result.ignored = !rule.flags.contains(IgnoreRuleFlags::NEGATED);
            result.matched_rule = Some(rule.clone());
        }
    }
    result
}

fn ignore_rule_matches(rule: &IgnoreRule, relative_path: &str, is_directory: bool) -> bool {
    let Some(path) = relative_to_ignore_rule_base(relative_path, &rule.base_path) else {
        return false;
    };
    if path.is_empty() {
        return false;
    }
    if rule.flags.contains(IgnoreRuleFlags::DIRECTORY_ONLY) {
        let directory = if is_directory {
            path
        } else {
            let Some((parent, _)) = path.rsplit_once('/') else {
                return false;
            };
            parent
        };
        if rule.flags.contains(IgnoreRuleFlags::ANCHORED)
            || rule.flags.contains(IgnoreRuleFlags::HAS_SLASH)
        {
            return rule.pattern.is_match(directory)
                || directory
                    .match_indices('/')
                    .any(|(end, _)| rule.pattern.is_match(&directory[..end]));
        }
        return directory
            .split('/')
            .any(|segment| rule.pattern.is_match(segment));
    }
    if rule.flags.contains(IgnoreRuleFlags::ANCHORED)
        || rule.flags.contains(IgnoreRuleFlags::HAS_SLASH)
    {
        return rule.pattern.is_match(path);
    }
    if is_directory
        && path
            .split('/')
            .any(|segment| rule.pattern.is_match(segment))
    {
        return true;
    }
    path.rsplit('/')
        .next()
        .is_some_and(|name| rule.pattern.is_match(name))
}

fn relative_to_ignore_rule_base<'a>(relative_path: &'a str, base_path: &str) -> Option<&'a str> {
    if base_path.is_empty() {
        return Some(relative_path);
    }
    if relative_path == base_path {
        return Some("");
    }
    relative_path.strip_prefix(&format!("{base_path}/"))
}

fn include_pattern_names_ignored_path(
    include: &PathPattern,
    relative_path: &str,
    ignored_pattern: &str,
) -> bool {
    if !include.might_match_descendant(relative_path) && !include.is_match(relative_path) {
        return false;
    }
    let ignored_segments: Vec<_> = ignored_pattern.split('/').collect();
    include.normalized().split('/').any(|segment| {
        !matches!(segment, "*" | "**" | "?")
            && ignored_segments.iter().any(|ignored| {
                segment_matches(segment, ignored) || segment_matches(ignored, segment)
            })
    })
}

fn segment_matches(pattern: &str, segment: &str) -> bool {
    PathPattern::path(pattern).is_ok_and(|compiled| compiled.is_match(segment))
}

fn include_pattern_declares_hidden_directory(pattern: &PathPattern, relative_path: &str) -> bool {
    let Some(name) = relative_path.rsplit('/').next() else {
        return true;
    };
    if !is_hidden_name(name) {
        return true;
    }
    pattern
        .normalized()
        .split('/')
        .any(|segment| segment.starts_with('.') && segment_matches(segment, name))
}

fn is_nested_git_repository_directory(path: &Path) -> bool {
    fs::metadata(path.join(".git")).is_ok_and(|metadata| metadata.is_file() || metadata.is_dir())
}

fn load_default_type_map() -> HashMap<String, Vec<String>> {
    let mut builder = TypesBuilder::new();
    builder.add_defaults();
    builder
        .definitions()
        .into_iter()
        .map(|definition| (definition.name().to_owned(), definition.globs().to_vec()))
        .collect()
}

fn resolve_type_names(
    names: &[String],
    type_map: &HashMap<String, Vec<String>>,
) -> Result<Vec<PathPattern>, HostError> {
    let aliases: HashMap<_, _> = RIPGREP_FILE_TYPE_ALIASES.into_iter().collect();
    let mut patterns = Vec::new();
    let mut seen = HashSet::new();
    for raw_name in names {
        let name = raw_name.trim().to_lowercase();
        if name.is_empty() {
            continue;
        }
        if name == "all" {
            if seen.insert("**".to_owned()) {
                patterns.push(PathPattern::rg("**", false)?);
            }
            continue;
        }
        let extension_name = name.strip_prefix('.').unwrap_or(&name);
        let resolved = if type_map.contains_key(&name) {
            name.as_str()
        } else {
            aliases.get(extension_name).copied().unwrap_or(&name)
        };
        let Some(type_patterns) = type_map.get(resolved) else {
            return Err(HostError::invalid_argument(format!(
                "unknown ripgrep file type: {raw_name}"
            )));
        };
        for pattern in type_patterns {
            if seen.insert(pattern.clone()) {
                patterns.push(PathPattern::rg(pattern, false)?);
            }
        }
    }
    Ok(patterns)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use crate::{DiscoveryOptions, RootSpec};
    use tempfile::tempdir;

    use super::{FileTypeResolver, RootPolicy, parse_gitignore_rules};

    fn root(discovery: DiscoveryOptions) -> RootSpec {
        RootSpec {
            path: PathBuf::from("/workspace"),
            recursive: true,
            discovery,
        }
    }

    #[test]
    fn gitignore_negation_and_explicit_include_match_typescript() {
        let resolver = FileTypeResolver::new();
        let policy = RootPolicy::new(
            root(DiscoveryOptions {
                include_paths: vec!["vendor/keep.ts".to_owned()],
                ..DiscoveryOptions::default()
            }),
            &resolver,
        )
        .expect("policy");
        let rules =
            parse_gitignore_rules("vendor/\n!vendor/keep.ts\n", "").expect("gitignore rules");
        assert!(policy.path_can_be_scanned("vendor/keep.ts", "keep.ts", false, &rules));
        assert!(!policy.path_can_be_scanned("vendor/drop.ts", "drop.ts", false, &rules));
    }

    #[test]
    fn directory_ignore_rules_do_not_exclude_files_with_the_same_name() {
        let policy = RootPolicy::new(root(DiscoveryOptions::default()), &FileTypeResolver::new())
            .expect("policy");
        let rules = policy.initial_ignore_rules();
        assert!(policy.path_can_be_scanned("tmp", "tmp", false, &rules));
        assert!(policy.path_can_be_scanned("notes/tmp", "tmp", false, &rules));
        assert!(!policy.path_can_be_scanned("tmp", "tmp", true, &rules));
        assert!(!policy.path_can_be_scanned("tmp/note.txt", "note.txt", false, &rules));
        let rules =
            parse_gitignore_rules("/nested/build*/\n", "").expect("anchored directory rule");
        assert!(policy.path_can_be_scanned("nested/build-output", "build-output", false, &rules));
        assert!(!policy.path_can_be_scanned(
            "nested/build-output/deep/note.txt",
            "note.txt",
            false,
            &rules
        ));
    }

    #[test]
    fn hidden_directories_require_hidden_or_an_explicit_include() {
        let resolver = FileTypeResolver::new();
        let defaults =
            RootPolicy::new(root(DiscoveryOptions::default()), &resolver).expect("default policy");
        assert!(!defaults.path_can_be_scanned(".vscode", ".vscode", true, &[]));

        let included = RootPolicy::new(
            root(DiscoveryOptions {
                include_paths: vec![".vscode/settings.json".to_owned()],
                ..DiscoveryOptions::default()
            }),
            &resolver,
        )
        .expect("included policy");
        assert!(included.path_can_be_scanned(".vscode", ".vscode", true, &[]));
    }

    #[test]
    fn gitignore_cache_is_invalidated_when_the_watched_file_changes() {
        let directory = tempdir().expect("policy root");
        let ignored = directory.path().join("volatile.txt");
        fs::write(directory.path().join(".gitignore"), "volatile.txt\n")
            .expect("initial ignore file");
        fs::write(&ignored, "fixture\n").expect("ignored fixture");
        let policy = RootPolicy::new(
            RootSpec {
                path: directory.path().to_path_buf(),
                recursive: true,
                discovery: DiscoveryOptions::default(),
            },
            &FileTypeResolver::new(),
        )
        .expect("policy");

        assert!(!policy.path_can_affect_index(&ignored, false));
        fs::write(directory.path().join(".gitignore"), "").expect("updated ignore file");
        policy.invalidate_gitignore_rules(directory.path());
        assert!(policy.path_can_affect_index(&ignored, false));
    }
}
