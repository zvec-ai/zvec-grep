use zg_cli::{ColorMode, OutputOptions, PreviewMode, write_context_with_options};
use zg_engine::api::context::ContextResult;

fn grouped_result() -> ContextResult {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("../../../compat/cli-search-presentation.json"))
            .expect("shared search fixtures");
    let mut value = fixtures["cases"][2]["result"].clone();
    value["query"] = "checkpoint".into();
    let mut primary = value["items"][0].clone();
    primary["content"] = "checkpoint workspace primary-source".into();
    let mut supplemental = primary.clone();
    supplemental["rank"] = 2.into();
    supplemental["content"] = "checkpoint workspace supplemental-source".into();
    value["items"] = serde_json::json!([primary]);
    value["group_results"] = serde_json::json!([
        {"id": "Q1", "query": "checkpoint", "role": "primary", "items": [primary]},
        {"id": "Q2", "query": "workspace", "role": "supplemental", "items": [supplemental]},
        {"id": "Q3", "query": "missing", "role": "supplemental", "items": []}
    ]);
    serde_json::from_value(value).expect("grouped result")
}

#[test]
fn every_preview_preserves_hits_and_local_ranks_without_search_metadata() {
    let result = grouped_result();
    let original = result.clone();
    for preview in [
        PreviewMode::None,
        PreviewMode::Short,
        PreviewMode::Full,
        PreviewMode::Detail,
    ] {
        for terminal in [false, true] {
            let mut bytes = Vec::new();
            write_context_with_options(
                &mut bytes,
                &result,
                OutputOptions {
                    preview,
                    color: ColorMode::Never,
                    ..OutputOptions::default()
                },
                terminal,
            )
            .expect("grouped render");
            let output = String::from_utf8(bytes).expect("UTF-8");
            for required in ["primary-source", "supplemental-source", "#1", "#2"] {
                assert!(
                    output.contains(required),
                    "{preview:?} missing {required}: {output}"
                );
            }
            for hidden in [
                "matchedBy=",
                "hits:",
                "Hits:",
                "query groups",
                "Groups:",
                "Group:",
                "Query:",
                "Routes:",
                "[primary]",
                "[supplemental]",
                "No matches",
            ] {
                assert!(!output.contains(hidden), "{preview:?}: {output}");
            }
            assert_eq!(
                output.matches("src/sample.ts").count(),
                2,
                "{preview:?}: {output}"
            );
            if matches!(preview, PreviewMode::Short | PreviewMode::Full) {
                assert!(
                    output.contains("  50: checkpoint workspace primary-source"),
                    "{output}"
                );
                assert!(
                    output.contains("  50: checkpoint workspace supplemental-source"),
                    "{output}"
                );
                assert!(!output.contains("source:"), "{output}");
            }
            assert_eq!(
                result, original,
                "rendering must preserve retrieval results"
            );
            let mut empty = result.clone();
            for group in &mut empty.group_results {
                group.items.clear();
            }
            let mut bytes = Vec::new();
            write_context_with_options(
                &mut bytes,
                &empty,
                OutputOptions {
                    preview,
                    color: ColorMode::Never,
                    ..OutputOptions::default()
                },
                terminal,
            )
            .expect("empty groups");
            let output = String::from_utf8(bytes).expect("UTF-8");
            assert_eq!(output.matches("No matches").count(), 1, "{output}");
            assert!(!output.contains("sample.ts"), "{output}");
        }
    }
}

#[test]
fn compact_previews_highlight_the_query_of_each_group() {
    let result = grouped_result();
    for preview in [PreviewMode::Short, PreviewMode::Full] {
        let mut bytes = Vec::new();
        write_context_with_options(
            &mut bytes,
            &result,
            OutputOptions {
                preview,
                color: ColorMode::Always,
                ..OutputOptions::default()
            },
            false,
        )
        .expect("colored grouped render");
        let output = String::from_utf8(bytes).expect("UTF-8");
        assert!(
            output.contains("  50: \x1b[1;33mcheckpoint\x1b[0m workspace primary-source"),
            "{preview:?}: {output}"
        );
        assert!(
            output.contains("  50: checkpoint \x1b[1;33mworkspace\x1b[0m supplemental-source"),
            "{preview:?}: {output}"
        );
    }
}

#[test]
fn short_and_full_keep_the_original_compact_layout_on_every_output() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("../../../compat/cli-search-presentation.json"))
            .expect("shared search fixtures");
    for (fixture_id, preview, expected) in [
        (
            "groups-outline-and-matched-window",
            PreviewMode::Short,
            concat!(
                "#1 src/sample.ts:10-33\n",
                "symbol: [function] resolveContext scope: Workspace\n",
                "  17: retrieved-source-line-8\n",
                "  18: retrieved-source-line-9\n",
                "  19: retrieved-source-line-10\n",
                "  20: retrieved-source-line-11\n",
                "  21: retrieved-source-line-12\n",
                "  22: retrieved-source-line-13\n",
                "  23: retrieved-source-line-14\n",
                "  24: retrieved-source-line-15\n",
                "  25: retrieved-source-line-16\n",
                "  26: retrieved-source-line-17\n",
                "  …\n\n",
                "#2 notes/design.md:file\n",
                "heading: Design\nheading_level: 1\nscope: Search / Design\n",
                "  # Design\n  \n  Retain local evidence.\n  \n",
            ),
        ),
        (
            "excerpt-only-source-and-redundant-symbol",
            PreviewMode::Full,
            concat!(
                "#1 src/sample.ts:1-100\n",
                "symbol: [function] resolveContext\n",
                "  50: function resolveContext() {\n",
                "  51:   return true;\n",
                "  52: }\n",
            ),
        ),
    ] {
        let fixture = fixtures["cases"]
            .as_array()
            .expect("cases")
            .iter()
            .find(|fixture| fixture["id"] == fixture_id)
            .expect("compact fixture");
        let result: ContextResult =
            serde_json::from_value(fixture["result"].clone()).expect("context result");
        for terminal in [false, true] {
            let mut bytes = Vec::new();
            write_context_with_options(
                &mut bytes,
                &result,
                OutputOptions {
                    preview,
                    color: ColorMode::Never,
                    ..OutputOptions::default()
                },
                terminal,
            )
            .expect("compact render");
            assert_eq!(String::from_utf8(bytes).expect("UTF-8"), expected);
        }
    }
}

#[test]
fn minimal_and_detailed_modes_match_fixtures_on_terminals_and_redirected_output() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("../../../compat/cli-search-presentation.json"))
            .expect("search presentation fixtures");
    for fixture in fixtures["cases"].as_array().expect("cases") {
        let result: ContextResult =
            serde_json::from_value(fixture["result"].clone()).expect("context result");
        let original = result.clone();
        for terminal in [false, true] {
            for (mode, preview) in [("none", PreviewMode::None), ("detail", PreviewMode::Detail)] {
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
                    fixture[format!("expected_rust_{mode}")]
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
