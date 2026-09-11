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
