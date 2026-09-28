//! Query-local structural signals; source text references are not name matches.
use super::Policy;
use crate::domain::{CodeMetadata, SymbolType};

#[derive(Default)]
// These independent features may all apply at once.
#[allow(clippy::struct_excessive_bools)]
pub(super) struct QueryFeatures {
    query_tokens: std::collections::HashSet<String>,
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
            paths: query
                .split_whitespace()
                .map(|s| s.trim_matches(['`', '\'', '"', ',', ';']))
                .filter(|s| s.contains(['/', '\\', '.']))
                .map(str::to_owned)
                .collect(),
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

    /// Match query terms against the file stem and nearby directories, once per term.
    pub(super) fn path_affinity(&self, path: &str) -> f64 {
        let normalized = path.replace('\\', "/");
        if self.paths.iter().any(|query| {
            let query = query.replace('\\', "/");
            let query = query.strip_prefix("./").unwrap_or(&query);
            normalized == query || normalized.ends_with(&format!("/{query}"))
        }) {
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
