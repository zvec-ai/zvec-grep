//! Query-local structural signals; source text references are not name matches.
use std::{borrow::Cow, collections::HashSet};

use super::Policy;
use crate::domain::{CodeMetadata, FileCategory, FileFormat, SymbolType};

#[derive(Default)]
// These independent features may all apply at once.
#[allow(clippy::struct_excessive_bools)]
pub(super) struct QueryFeatures {
    query_tokens: HashSet<String>,
    scope_names: HashSet<String>,
    file_stems: HashSet<String>,
    pub(super) paths: Vec<String>,
    explicit_names: Vec<String>,
    pub(super) usages: bool,
    pub(super) tests: bool,
    pub(super) examples: bool,
    pub(super) dependencies: bool,
    pub(super) generated: bool,
    requested_kind: Option<SymbolType>,
    pure_symbol: bool,
    implementations: bool,
}

impl QueryFeatures {
    pub(super) fn new(query: &str) -> Self {
        let identifiers = identifiers(query);
        let lower = query.to_lowercase();
        let words = identifiers
            .iter()
            .map(|s| s.to_lowercase())
            .collect::<Vec<_>>();
        let has = |word: &str| words.iter().any(|s| s == word);
        let mut explicit_names = query
            .split('`')
            .enumerate()
            .filter(|(i, _)| i % 2 == 1)
            .flat_map(|(_, s)| identifiers_in(s))
            .collect::<Vec<_>>();
        explicit_names.extend(
            identifiers
                .iter()
                .filter(|s| {
                    identifiers.len() == 1
                        || s.contains(['_', ':', '.'])
                        || s.chars().any(char::is_uppercase)
                })
                .cloned(),
        );
        explicit_names.sort();
        explicit_names.dedup();
        let mut generic_names = HashSet::new();
        let anchor_query = without_type_arguments(query, Some(&mut generic_names));
        let (scope_names, file_stems) = function_anchor_names(&anchor_query, &generic_names);
        let requested_kind = [
            ("class", SymbolType::Class),
            ("function", SymbolType::Function),
            ("interface", SymbolType::Interface),
            ("trait", SymbolType::Interface),
            ("enum", SymbolType::Enum),
            ("alias", SymbolType::Alias),
            ("constant", SymbolType::Value),
            ("field", SymbolType::Value),
        ]
        .into_iter()
        .find_map(|(word, kind)| has(word).then_some(kind));
        Self {
            implementations: has("implementations")
                || lower.contains("实现类")
                || lower.contains("所有实现"),
            pure_symbol: identifiers.len() == 1
                && explicit_names.iter().any(|name| {
                    query.contains('`')
                        || name.contains(['_', ':', '.'])
                        || name.chars().any(char::is_uppercase)
                }),
            query_tokens: identifiers
                .iter()
                .flat_map(|s| tokens(s))
                .filter(|s| informative(s))
                .collect(),
            paths: anchor_query
                .split_whitespace()
                .map(|s| s.trim_matches(['`', '\'', '"', ',', ';']))
                .filter(|s| s.contains(['/', '\\', '.']))
                .map(str::to_owned)
                .collect(),
            scope_names,
            file_stems,
            explicit_names,
            requested_kind,
            usages: ["usage", "usages", "callers", "references"]
                .iter()
                .any(|word| has(word))
                || ["调用方", "谁调用", "谁使用", "引用位置"]
                    .iter()
                    .any(|s| lower.contains(s)),
            tests: [
                "unit test",
                "unit tests",
                "test case",
                "test cases",
                "test file",
                "test files",
                "tests for",
                "tests of",
                "单元测试",
                "测试用例",
                "测试文件",
            ]
            .iter()
            .any(|s| lower.contains(s)),
            examples: [
                "example", "examples", "fixture", "fixtures", "mock", "mocks",
            ]
            .iter()
            .any(|word| has(word))
                || lower.contains("示例"),
            dependencies: ["vendor", "thirdparty", "third_party", "node_modules"]
                .iter()
                .any(|word| has(word))
                || lower.contains("third-party")
                || lower.contains("third party")
                || lower.contains("dependency source")
                || lower.contains("依赖库")
                || lower.contains("第三方"),
            generated: lower.contains("generated code") || lower.contains("生成代码"),
        }
    }

    pub(super) fn describe(&self) -> Vec<String> {
        let mut features = Vec::new();
        if !self.explicit_names.is_empty() {
            features.push("identifier".to_owned());
        }
        if !self.paths.is_empty() {
            features.push("path".to_owned());
        }
        for (enabled, name) in [
            (self.pure_symbol, "pure_symbol"),
            (self.implementations, "implementations"),
            (self.usages, "usages"),
            (self.tests, "tests"),
            (self.examples, "examples"),
            (self.dependencies, "dependencies"),
            (self.generated, "generated"),
        ] {
            if enabled {
                features.push(name.to_owned());
            }
        }
        features
    }

    #[cfg(test)]
    pub(super) fn symbol_bonus(&self, code: &CodeMetadata) -> (&'static str, f64, f64) {
        self.symbol_bonus_with(code, Policy::CURRENT)
    }

    pub(super) fn symbol_bonus_with(
        &self,
        code: &CodeMetadata,
        policy: Policy,
    ) -> (&'static str, f64, f64) {
        let Some(name) = code.symbol_name.as_deref().filter(|s| !s.is_empty()) else {
            return ("none", 0.0, 0.0);
        };
        let scope = code.scope.as_deref().unwrap_or_default().replace('.', "::");
        let name = name.replace('.', "::");
        let qualified = if scope.is_empty() {
            name.clone()
        } else {
            format!("{scope}::{name}")
        };
        let mut best = ("none", 0.0_f64);
        for query_name in &self.explicit_names {
            let query_name = query_name.replace('.', "::");
            let bonus = if query_name.contains("::") && query_name == qualified {
                ("qualified_exact", 0.35)
            } else if query_name == name && informative(&query_name) {
                ("exact", 0.25)
            } else if query_name.to_lowercase() == name.to_lowercase() && informative(&query_name) {
                ("case_folded", 0.10)
            } else {
                ("none", 0.0)
            };
            if bonus.1 > best.1 {
                best = bonus;
            }
        }
        if best.1 == 0.0 {
            let name_tokens = tokens(&name);
            let matched = name_tokens
                .iter()
                .filter(|s| informative(s) && self.query_tokens.contains(*s))
                .count();
            if matched > 0 {
                // Bound conversion by the identifier token count rather than query length.
                let numerator = f64::from(u32::try_from(matched).unwrap_or(u32::MAX));
                let denominator =
                    f64::from(u32::try_from(name_tokens.len()).unwrap_or(u32::MAX)).max(1.0);
                best = ("tokens", 0.10 * numerator / denominator);
                if policy.weak_names {
                    let query_size = count_as_f64(self.query_tokens.len()).max(1.0);
                    let distinct = name_tokens
                        .iter()
                        .filter(|s| informative(s) && self.query_tokens.contains(*s))
                        .collect::<std::collections::HashSet<_>>()
                        .len();
                    let coverage = (count_as_f64(distinct) / query_size).min(1.0);
                    // Weak property names need query coverage; explicit constants stay exact.
                    let reliability = if code.symbol_type == Some(SymbolType::Value)
                        && self.requested_kind != Some(SymbolType::Value)
                    {
                        0.25
                    } else {
                        1.0
                    };
                    best.1 *= coverage.sqrt() * reliability;
                }
            } else if !scope.is_empty() && self.explicit_names.iter().any(|s| s == &scope) {
                best = ("scope", 0.03);
            }
        }
        let definition = if self.usages || !matches!(best.0, "exact" | "qualified_exact") {
            0.0
        } else if let Some(requested) = self.requested_kind {
            if code.symbol_type == Some(requested) {
                0.10
            } else {
                0.0
            }
        } else if matches!(
            code.symbol_type,
            Some(
                SymbolType::Class
                    | SymbolType::Function
                    | SymbolType::Interface
                    | SymbolType::Alias
                    | SymbolType::Enum
                    | SymbolType::Module
            )
        ) {
            0.10
        } else {
            0.0
        };
        let definition = if policy.definition_intent && definition > 0.0 {
            if self.implementations && code.symbol_type == Some(SymbolType::Interface) {
                0.0
            } else if self.pure_symbol {
                0.85
            } else if self.explicit_names.len() == 1 {
                0.40
            } else {
                definition
            }
        } else {
            definition
        };
        (best.0, best.1, definition)
    }

    pub(super) fn names_function_file_or_scope(&self, path: &str, scope: Option<&str>) -> bool {
        if scope.is_some_and(|scope| {
            let scope = scope_type_name(scope);
            self.scope_names.contains(scope.as_ref())
                || scope
                    .match_indices("::")
                    .any(|(i, _)| self.scope_names.contains(&scope[i + 2..]))
        }) {
            return true;
        }
        let path = path.replace('\\', "/");
        self.paths.iter().any(|query| {
            let literal = query
                .trim_start_matches(['(', '[', '{'])
                .trim_end_matches([')', ']', '}', '.', '?', '!', ':', '`', '\'', '"']);
            path_matches(&path, literal)
        }) || (!self.file_stems.is_empty()
            && std::path::Path::new(&path)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| self.file_stems.contains(&normalized_stem(stem))))
    }

    /// Match query terms against the file stem and nearby directories, once per term.
    pub(super) fn path_affinity(&self, path: &str) -> f64 {
        let normalized = path.replace('\\', "/");
        if self
            .paths
            .iter()
            .any(|query| path_matches(&normalized, query))
        {
            return 1.0;
        }
        let parts = normalized.split('/').collect::<Vec<_>>();
        let file = parts.last().copied().unwrap_or_default();
        let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
        let stem_tokens = tokens(stem)
            .into_iter()
            .filter(|s| path_word(s))
            .collect::<Vec<_>>();
        let directory_tokens = parts
            .iter()
            .rev()
            .skip(1)
            .take(2)
            .flat_map(|part| tokens(part))
            .filter(|s| path_word(s))
            .collect::<Vec<_>>();
        let query_tokens = self
            .query_tokens
            .iter()
            .filter(|s| path_word(s))
            .collect::<Vec<_>>();
        if query_tokens.is_empty() {
            return 0.0;
        }
        let sum = query_tokens
            .iter()
            .map(|word| {
                if stem_tokens.iter().any(|part| related(word, part)) {
                    1.0
                } else if directory_tokens.iter().any(|part| related(word, part)) {
                    0.5
                } else {
                    0.0
                }
            })
            .sum::<f64>();
        sum / count_as_f64(query_tokens.len()).max(1.0)
    }
}

fn function_anchor_names(
    query: &str,
    generic_names: &HashSet<&str>,
) -> (HashSet<String>, HashSet<String>) {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut scope_names = HashSet::new();
    let mut standalone_names: HashSet<&str> = query
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|word| !word.is_empty() && word.chars().all(is_word))
        .collect();
    let terms = query.split(|c: char| c.is_whitespace() || "`'\"(),;[]{}".contains(c));
    for (i, term) in terms.filter(|term| !term.is_empty()).enumerate() {
        // Paths and recognized source filenames are matched literally, as a whole.
        let literal = term.trim_end_matches(['.', '?', '!', ':']);
        if term.contains(['/', '\\'])
            || (literal.contains('.')
                && FileFormat::from_file_name(literal.as_ref())
                    .iter()
                    .any(|format| format.categories().contains(&FileCategory::Code)))
        {
            continue;
        }
        let words = term.split(|c| !is_word(c) && !matches!(c, ':' | '.'));
        for (j, raw) in words.filter(|word| word.chars().any(is_word)).enumerate() {
            let word = raw.trim_matches([':', '.']);
            if raw.contains("::") || word.contains('.') {
                let qualified = word.replace('.', "::");
                // Member syntax can also name the enclosing type (Router.Dial or Router::dial).
                if let Some((owner, member)) = qualified.rsplit_once("::")
                    && !raw.ends_with("::")
                    && owner
                        .rsplit("::")
                        .next()
                        .is_some_and(|name| name.starts_with(char::is_uppercase))
                    && (word.contains('.') || member.starts_with(char::is_lowercase))
                {
                    scope_names.insert(owner.to_owned());
                }
                scope_names.insert(qualified);
            } else if generic_names.contains(word)
                || word.contains('_')
                || word.chars().skip(1).any(char::is_uppercase)
                // A capitalized first word in a sentence is often just prose.
                || ((i > 0 || j > 0 || query.trim() == word) && word.starts_with(char::is_uppercase))
            {
                standalone_names.insert(word);
            }
        }
    }
    let file_stems = standalone_names
        .iter()
        .filter(|word| word.chars().count() >= 3)
        .map(|word| normalized_stem(word))
        .filter(|stem| !stem.is_empty())
        .collect();
    scope_names.extend(standalone_names.into_iter().map(str::to_owned));
    (scope_names, file_stems)
}

/// Keep the qualified type name from extracted scopes such as `jsonc::Parser<'a>`.
fn scope_type_name(scope: &str) -> Cow<'_, str> {
    let name = without_type_arguments(scope, None);
    if name.contains('.') {
        Cow::Owned(name.replace('.', "::"))
    } else {
        name
    }
}

/// Elide balanced argument lists. In queries, also record explicitly named types
/// so the sentence-initial filter can distinguish `Vec<T>` from a prose word.
fn without_type_arguments<'a>(
    text: &'a str,
    mut query_names: Option<&mut HashSet<&'a str>>,
) -> Cow<'a, str> {
    if !text.contains('<') {
        return Cow::Borrowed(text);
    }
    let mut result = String::new();
    let mut copied = 0;
    let mut prefix = "";
    let mut name = "";
    let mut depth = 0;
    for (i, ch) in text.char_indices() {
        match ch {
            '<' if depth > 0 => depth += 1,
            '<' => {
                let before = &text[copied..i];
                prefix = before.trim_end().trim_end_matches("::");
                name = prefix
                    .rsplit(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or_default();
                // A spaced comparison in prose should leave the following terms visible.
                if query_names.is_none()
                    || (!name.is_empty()
                        && (!before.ends_with(char::is_whitespace)
                            || name.starts_with(char::is_uppercase)))
                {
                    depth = 1;
                }
            }
            '>' if depth > 0 && !text[..i].ends_with('-') => {
                depth -= 1;
                if depth == 0 {
                    if let Some(names) = query_names.as_deref_mut() {
                        names.insert(name);
                    }
                    result.push_str(prefix);
                    copied = i + 1;
                }
            }
            _ => {}
        }
    }
    if copied == 0 {
        Cow::Borrowed(text)
    } else {
        result.push_str(&text[copied..]);
        Cow::Owned(result)
    }
}

fn normalized_stem(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn path_matches(normalized: &str, query: &str) -> bool {
    let query = query.replace('\\', "/");
    let query = query.strip_prefix("./").unwrap_or(&query);
    normalized == query
        || normalized
            .strip_suffix(query)
            .is_some_and(|prefix| prefix.ends_with('/'))
}

fn count_as_f64(count: usize) -> f64 {
    f64::from(u32::try_from(count).unwrap_or(u32::MAX))
}

fn path_word(word: &str) -> bool {
    informative(word)
        && !matches!(
            word,
            "src"
                | "lib"
                | "index"
                | "mod"
                | "main"
                | "test"
                | "tests"
                | "spec"
                | "examples"
                | "vendor"
                | "node_modules"
                | "into"
                | "onto"
                | "can"
                | "its"
                | "not"
                | "when"
                | "where"
        )
}

fn related(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    // A narrow prefix allowance handles plurals and short inflections, not substrings.
    let (short, long) = if a.len() < b.len() { (a, b) } else { (b, a) };
    short.chars().count() >= 4
        && long.starts_with(short)
        && long.chars().count() - short.chars().count() <= 3
}

fn identifiers(query: &str) -> Vec<String> {
    let mut result = identifiers_in(query).collect::<Vec<_>>();
    result.sort();
    result.dedup();
    result
}

fn identifiers_in(query: &str) -> impl Iterator<Item = String> + '_ {
    query
        .split(|c: char| !c.is_alphanumeric() && !matches!(c, '_' | '$' | ':' | '.'))
        .map(|s| s.trim_matches([':', '.']))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn informative(s: &str) -> bool {
    s.chars().count() >= 3
        && !matches!(
            s.to_lowercase().as_str(),
            "get"
                | "set"
                | "new"
                | "the"
                | "and"
                | "for"
                | "how"
                | "does"
                | "are"
                | "with"
                | "from"
                | "this"
                | "that"
                | "class"
                | "function"
                | "interface"
        )
}

fn tokens(s: &str) -> Vec<String> {
    let chars = s.chars().collect::<Vec<_>>();
    let mut result = Vec::new();
    let mut word = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !word.is_empty() {
                result.push(std::mem::take(&mut word).to_lowercase());
            }
            continue;
        }
        let boundary = c.is_uppercase()
            && i > 0
            && (chars[i - 1].is_lowercase()
                || (chars[i - 1].is_uppercase()
                    && chars.get(i + 1).is_some_and(|next| next.is_lowercase())));
        if boundary && !word.is_empty() {
            result.push(std::mem::take(&mut word).to_lowercase());
        }
        word.push(c);
    }
    if !word.is_empty() {
        result.push(word.to_lowercase());
    }
    result
}
