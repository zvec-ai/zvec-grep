//! Query-sensitive weights for paired FTS/vector reciprocal-rank fusion.
use std::collections::HashMap;

use super::pipeline::ResolvedSearchRoute;
use crate::api::context::options::ContextRouteMode as SearchRouteMode;

pub(super) fn route_weights(routes: &[ResolvedSearchRoute]) -> HashMap<String, f64> {
    routes
        .iter()
        .map(|route| {
            // Only rebalance matching hybrid queries. Independent routes and
            // explicit single-mode requests retain their original contribution.
            let paired = routes
                .iter()
                .any(|other| other.mode != route.mode && other.query == route.query);
            let strength = if paired {
                symbol_strength(&route.query)
            } else {
                0.0
            };
            let weight = match route.mode {
                SearchRouteMode::Fts => 1.0 + strength,
                SearchRouteMode::Vector => 1.0 - strength,
            };
            (route.id.clone(), weight)
        })
        .collect()
}

fn symbol_strength(query: &str) -> f64 {
    let tokens = query
        .split(|c: char| {
            !c.is_ascii_alphanumeric() && !matches!(c, '_' | '$' | ':' | '.' | '/' | '\\')
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    let bare = query.trim().trim_matches('`').trim_end_matches("()");
    for raw in tokens {
        let token = raw.trim_matches([':', '.']);
        if token.contains(['/', '\\']) || !valid_name(token) || stopword(token) {
            continue;
        }
        if token.rsplit_once('.').is_some_and(|(_, ext)| {
            matches!(
                ext,
                "rs" | "ts"
                    | "tsx"
                    | "js"
                    | "jsx"
                    | "py"
                    | "go"
                    | "java"
                    | "md"
                    | "json"
                    | "toml"
                    | "yaml"
                    | "cpp"
                    | "h"
                    | "cs"
            )
        }) {
            continue;
        }
        let structured = token.contains("::") || token.contains('.') || token.contains('_');
        let camel = token
            .as_bytes()
            .windows(2)
            .any(|pair| pair[0].is_ascii_lowercase() && pair[1].is_ascii_uppercase());
        let explicit =
            query.contains(&format!("`{token}`")) || query.contains(&format!("{token}("));
        if bare == token
            && (structured || explicit || token.chars().any(|c| c.is_ascii_uppercase()))
        {
            // Preserve total weight two: 1.4 / 0.6 is a 70 / 30 hybrid mix.
            return 0.4;
        }
        if structured || camel || explicit {
            // An identifier inside prose may be incidental to the user's intent.
            return 0.2;
        }
    }
    0.0
}

fn valid_name(name: &str) -> bool {
    name.len() > 1
        && name.split("::").flat_map(|s| s.split('.')).all(|part| {
            part.starts_with(|c: char| c.is_ascii_alphabetic() || matches!(c, '_' | '$'))
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$'))
                && part.chars().any(|c| c.is_ascii_alphanumeric())
        })
}

fn stopword(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "where"
            | "what"
            | "why"
            | "when"
            | "which"
            | "how"
            | "find"
            | "show"
            | "explain"
            | "the"
            | "this"
            | "that"
            | "class"
            | "struct"
            | "trait"
            | "interface"
            | "function"
            | "method"
            | "type"
            | "enum"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_pure_and_embedded_symbols_without_sentence_case_false_positives() {
        for query in [
            "RequestService",
            "requestService",
            "request_service",
            "Request::Service",
            "Request.Service",
            "Visitor",
            "`serialize`",
            "serialize()",
            "_private",
        ] {
            assert!((symbol_strength(query) - 0.4).abs() < 1e-12, "{query}");
        }
        for query in [
            "Where is RequestService defined?",
            "find request_service",
            "解释 Request::Service",
            "how `serialize` works",
            "call serialize()",
            "find Request.Service",
        ] {
            assert!((symbol_strength(query) - 0.2).abs() < 1e-12, "{query}");
        }
        for query in [
            "how requests are handled",
            "Where is the service?",
            "Why does HTTP work?",
            "session",
            "Where",
            "src/RequestService.rs",
            r"src\RequestService.rs",
            "https://example.com",
            "RequestService.ts",
            "version 1.2",
            "__",
        ] {
            assert!((symbol_strength(query) - 0.0).abs() < 1e-12, "{query}");
        }
    }

    #[test]
    fn only_rebalances_paired_routes_and_preserves_total_weight() {
        let route = |id: &str, mode, query: &str| ResolvedSearchRoute {
            id: id.into(),
            mode,
            query: query.into(),
        };
        let routes = vec![
            route("fts", SearchRouteMode::Fts, "RequestService"),
            route("vector", SearchRouteMode::Vector, "RequestService"),
            route("fts-2", SearchRouteMode::Fts, "request_service"),
            route("vector-2", SearchRouteMode::Vector, "handle requests"),
        ];
        let weights = route_weights(&routes);
        assert!((weights["fts"] - 1.4).abs() < 1e-12);
        assert!((weights["vector"] - 0.6).abs() < 1e-12);
        assert!((weights["fts"] + weights["vector"] - 2.0).abs() < 1e-12);
        assert!((weights["fts-2"] - 1.0).abs() < 1e-12);
        assert!((weights["vector-2"] - 1.0).abs() < 1e-12);
        for route in routes {
            assert!((route_weights(std::slice::from_ref(&route))[&route.id] - 1.0).abs() < 1e-12);
        }
    }
}
