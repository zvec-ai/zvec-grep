use std::io::{self, BufRead, Write};
use zg_engine::authorization::IndexAuthorization;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorizationDecision {
    Once,
    Workspace,
    Cancel,
}

/// Displays the remote indexing disclosure and reads one explicit decision.
///
/// # Errors
/// Returns terminal I/O errors. Empty, invalid, and EOF input cancel the operation.
pub fn prompt_index_authorization(
    target: &IndexAuthorization,
    mut input: impl BufRead,
    mut output: impl Write,
) -> io::Result<AuthorizationDecision> {
    let roots = target
        .workspace_roots
        .iter()
        .take(2)
        .map(|root| {
            label(
                &root
                    .file_name()
                    .unwrap_or(root.as_os_str())
                    .to_string_lossy(),
                32,
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let extra = if target.workspace_roots.len() > 2 {
        format!(" +{}", target.workspace_roots.len() - 2)
    } else {
        String::new()
    };
    writeln!(
        output,
        "Remote Embedding authorization\n\nSend selected workspace files?\n\n  From  {roots}{extra}\n  To    {}\n        {}\n\nAPI charges may apply.\n\n1. Allow once\n2. Allow for this workspace\n3. Cancel",
        label(&target.model, 72),
        label(&target.endpoint_host, 72)
    )?;
    write!(output, "Choose [1-3]: ")?;
    output.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(match answer.trim() {
        "1" => AuthorizationDecision::Once,
        "2" => AuthorizationDecision::Workspace,
        _ => AuthorizationDecision::Cancel,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryAuthorizationDecision {
    Once,
    Workspace,
    FtsOnly,
    Cancel,
}

/// Displays query-specific consent choices before any remote operation.
/// # Errors
/// Returns terminal I/O errors. Empty, invalid, and EOF input cancel.
pub fn prompt_query_authorization(
    target: &IndexAuthorization,
    query_text: bool,
    workspace_content: bool,
    mut input: impl BufRead,
    mut output: impl Write,
) -> io::Result<QueryAuthorizationDecision> {
    let data = match (query_text, workspace_content) {
        (true, true) => "query text and selected workspace files",
        (false, true) => "selected workspace files",
        _ => "query text",
    };
    let root = label(
        &target
            .root
            .file_name()
            .unwrap_or(target.root.as_os_str())
            .to_string_lossy(),
        32,
    );
    writeln!(
        output,
        "Remote Embedding authorization\n\nSend {data}?\n\n  From  {root}\n  To    {}\n        {}\n\nAPI charges may apply.\n\n1. Allow once\n2. Allow for this workspace\n3. Use FTS only\n4. Cancel",
        label(&target.model, 72),
        label(&target.endpoint_host, 72)
    )?;
    write!(output, "Choose [1-4]: ")?;
    output.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(match answer.trim() {
        "1" => QueryAuthorizationDecision::Once,
        "2" => QueryAuthorizationDecision::Workspace,
        "3" => QueryAuthorizationDecision::FtsOnly,
        _ => QueryAuthorizationDecision::Cancel,
    })
}

/// Converts every route to FTS and prevents any refresh from sending file data.
pub fn use_fts_only(request: &mut zg_engine::api::context::ContextOptions) {
    use zg_engine::api::context::options::{ContextRoute, ContextRouteMode, RefreshPolicy};
    let queries = request
        .query
        .take()
        .into_iter()
        .chain(std::mem::take(&mut request.queries));
    request.routes.extend(queries.map(|query| ContextRoute {
        mode: ContextRouteMode::Fts,
        query,
    }));
    for route in &mut request.routes {
        route.mode = ContextRouteMode::Fts;
    }
    request.refresh = Some(RefreshPolicy::Off);
    request.auto_update = false;
    request.allow_remote = false;
}

fn label(value: &str, limit: usize) -> String {
    let clean: String = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if clean.chars().count() <= limit {
        clean
    } else {
        clean.chars().take(limit - 1).chain(['…']).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_fallback_preserves_queries_and_filters_and_disables_refresh() {
        use zg_engine::api::context::{
            ContextOptions,
            options::{ContextRoute, ContextRouteMode, RefreshPolicy},
        };
        let mut request = ContextOptions {
            query: Some("primary".into()),
            queries: vec!["second".into()],
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Vector,
                query: "vector".into(),
            }],
            limit: Some(7),
            globs: vec!["*.rs".into()],
            fuse: true,
            refresh: Some(RefreshPolicy::Wait),
            ..ContextOptions::default()
        };
        use_fts_only(&mut request);
        assert!(request.query.is_none() && request.queries.is_empty());
        assert_eq!(
            request
                .routes
                .iter()
                .map(|r| r.query.as_str())
                .collect::<Vec<_>>(),
            ["vector", "primary", "second"]
        );
        assert!(
            request
                .routes
                .iter()
                .all(|r| r.mode == ContextRouteMode::Fts)
        );
        assert_eq!(request.limit, Some(7));
        assert_eq!(request.globs, ["*.rs"]);
        assert!(request.fuse);
        assert!(!request.auto_update && !request.allow_remote);
        assert_eq!(request.refresh, Some(RefreshPolicy::Off));
    }

    #[test]
    fn query_prompt_offers_fts_without_granting_remote_consent() {
        let target = IndexAuthorization {
            root: std::env::temp_dir().join("docs"),
            workspace_roots: vec![std::env::temp_dir().join("docs")],
            model: "qwen/text-embedding-v4".into(),
            endpoint: "https://dashscope.aliyuncs.com/embeddings".into(),
            endpoint_host: "dashscope.aliyuncs.com".into(),
        };
        for (answer, expected) in [
            ("1\n", QueryAuthorizationDecision::Once),
            ("2\n", QueryAuthorizationDecision::Workspace),
            ("3\n", QueryAuthorizationDecision::FtsOnly),
            ("4\n", QueryAuthorizationDecision::Cancel),
            ("invalid\n", QueryAuthorizationDecision::Cancel),
            ("\n", QueryAuthorizationDecision::Cancel),
            ("", QueryAuthorizationDecision::Cancel),
        ] {
            let mut output = Vec::new();
            assert_eq!(
                prompt_query_authorization(&target, true, false, answer.as_bytes(), &mut output)
                    .expect("prompt"),
                expected
            );
        }
        let mut output = Vec::new();
        let decision =
            prompt_query_authorization(&target, true, false, "3\n".as_bytes(), &mut output)
                .expect("prompt");
        assert_eq!(decision, QueryAuthorizationDecision::FtsOnly);
        let text = String::from_utf8(output).expect("UTF-8");
        assert!(text.contains("Send query text?"));
        assert!(text.contains("3. Use FTS only\n4. Cancel\nChoose [1-4]: "));
    }

    #[test]
    fn matches_legacy_prompt_and_requires_explicit_consent() {
        let target = IndexAuthorization {
            root: "/workspace/docs".into(),
            workspace_roots: vec!["/workspace/docs".into()],
            model: "qwen/text-embedding-v4".into(),
            endpoint: "https://dashscope.aliyuncs.com/embeddings".into(),
            endpoint_host: "dashscope.aliyuncs.com".into(),
        };
        for (answer, expected) in [
            ("1\n", AuthorizationDecision::Once),
            ("2\n", AuthorizationDecision::Workspace),
            ("3\n", AuthorizationDecision::Cancel),
            ("\n", AuthorizationDecision::Cancel),
            ("invalid\n", AuthorizationDecision::Cancel),
            ("", AuthorizationDecision::Cancel),
        ] {
            let mut output = Vec::new();
            assert_eq!(
                prompt_index_authorization(&target, answer.as_bytes(), &mut output)
                    .expect("prompt"),
                expected
            );
            let text = String::from_utf8(output).expect("UTF-8 prompt");
            assert!(text.contains("Send selected workspace files?\n\n  From  docs\n  To    qwen/text-embedding-v4\n        dashscope.aliyuncs.com"));
            assert!(text.ends_with(
                "1. Allow once\n2. Allow for this workspace\n3. Cancel\nChoose [1-3]: "
            ));
        }
    }
}
