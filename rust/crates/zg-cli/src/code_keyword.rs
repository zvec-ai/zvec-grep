//! Conservative routing for standalone, unambiguous code identifiers.

use crate::QueryArgs;
use zg_engine::api::context::{
    ContextOptions,
    options::{QueryFilter, RefreshPolicy},
};

pub(crate) fn configure_literal_search(request: &mut ContextOptions, enabled: bool) {
    if !enabled {
        return;
    }
    request.rg = true;
    request.limit = Some(request.limit.unwrap_or(7));
    request.auto_update = false;
    request.refresh = Some(RefreshPolicy::Off);
    request.rg_options.fixed_strings = true;
    request.rg_options.word_regexp = true;
    request.filter = QueryFilter::default();
}

pub(crate) fn uses_literal_search(args: &QueryArgs) -> bool {
    !args.rg
        && args.values.len() == 1
        && args.hybrid_queries.is_empty()
        && args.fts.is_empty()
        && args.vector.is_empty()
        && !args.fuse
        && !args.prefer_symbol
        && args.symbol_types.is_empty()
        && args.refresh.is_none_or(|refresh| refresh == crate::RefreshMode::Off)
        // These filters describe indexed metadata, not filesystem matches.
        && args.files.file_types.is_empty()
        && args.files.excluded_file_types.is_empty()
        && args.files.categories.is_empty()
        && args.files.excluded_categories.is_empty()
        && is_code_keyword(args.values[0].trim())
}

fn is_code_keyword(query: &str) -> bool {
    let segments = query.split("::").collect::<Vec<_>>();
    if !segments.iter().all(|segment| {
        let mut bytes = segment.bytes();
        bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    }) {
        return false;
    }
    if segments.len() > 1 {
        return true;
    }
    let bytes = query.as_bytes();
    // Require meaningful words on both sides of an underscore. Leading/trailing
    // underscores and plain capitalized words alone are weak identifier evidence.
    let snake_case = query.contains('_')
        && query
            .split('_')
            .all(|word| !word.is_empty() && word.bytes().any(|byte| byte.is_ascii_alphabetic()));
    let camel_case = bytes
        .windows(2)
        .any(|pair| pair[0].is_ascii_lowercase() && pair[1].is_ascii_uppercase())
        || bytes.windows(3).any(|triple| {
            triple[0].is_ascii_uppercase()
                && triple[1].is_ascii_uppercase()
                && triple[2].is_ascii_lowercase()
        });
    snake_case || camel_case
}

#[cfg(test)]
mod tests {
    use super::is_code_keyword;

    #[test]
    fn recognizes_standalone_structured_identifiers() {
        for query in [
            "verifyToken",
            "RequestService",
            "request_service",
            "REQUEST_SERVICE",
            "Request::Service",
            "std::io::Read",
            "HTTPRequest",
            "parseHTTP2",
        ] {
            assert!(is_code_keyword(query), "{query}");
        }
    }

    #[test]
    fn leaves_ambiguous_words_sentences_paths_and_patterns_unchanged() {
        for query in [
            "",
            "Request",
            "request",
            "HTTP",
            "schema",
            "verify token",
            "how verifyToken works",
            "request-service",
            "request/service",
            "request.service",
            "src/request_service.rs",
            "verifyToken()",
            "verifyToken.*",
            "`verifyToken`",
            "Request::",
            "::Service",
            "Request:::Service",
            "request__service",
            "request_",
            "_request",
            "request_2",
            "123Request",
            "验证Token",
        ] {
            assert!(!is_code_keyword(query), "{query}");
        }
    }
}
