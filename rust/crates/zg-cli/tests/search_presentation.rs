use zg_cli::{ColorMode, OutputOptions, PreviewMode, write_context_with_options};
use zg_engine::api::context::ContextResult;

#[test]
fn all_preview_modes_match_node_on_terminals_and_redirected_output() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("../../../compat/cli-search-presentation.json"))
            .expect("shared Node.js fixtures");
    for fixture in fixtures["cases"].as_array().expect("cases") {
        let result: ContextResult =
            serde_json::from_value(fixture["result"].clone()).expect("context result");
        let original = result.clone();
        for terminal in [false, true] {
            for (mode, preview) in [
                ("none", PreviewMode::None),
                ("short", PreviewMode::Short),
                ("full", PreviewMode::Full),
                ("detail", PreviewMode::Detail),
            ] {
                let mut bytes = Vec::new();
                write_context_with_options(
                    &mut bytes,
                    &result,
                    OutputOptions {
                        preview,
                        color: ColorMode::Never,
                        trace: fixture["trace"].as_bool().unwrap_or(false),
                        ..OutputOptions::default()
                    },
                    terminal,
                )
                .expect("render");
                let actual = String::from_utf8(bytes).expect("UTF-8");
                let expected = format!(
                    "{}\n",
                    fixture[format!("expected_{mode}")]
                        .as_str()
                        .expect("expected output")
                );
                assert_eq!(
                    actual, expected,
                    "{} {mode} terminal={terminal}",
                    fixture["id"]
                );
                assert_eq!(
                    result, original,
                    "presentation must not mutate retrieval results"
                );
            }
        }
    }
}
