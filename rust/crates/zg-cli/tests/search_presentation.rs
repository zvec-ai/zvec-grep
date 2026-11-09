use zg_cli::{ColorMode, OutputOptions, PreviewMode, write_context_with_options};
use zg_engine::api::context::{ContextResult, result::ContentRange};

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
fn every_preview_separates_queries_and_preserves_hits_and_local_ranks() {
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
            let first = output
                .find("-- query 1/3: checkpoint --\n\n")
                .expect("first query");
            let second = output
                .find("-- query 2/3: workspace --\n\n")
                .expect("second query");
            assert!(first < output.find("primary-source").expect("first hit"));
            assert!(output.find("primary-source").expect("first hit") < second);
            assert!(second < output.find("supplemental-source").expect("second hit"));
            assert!(!output.contains("query 3/3:"), "empty groups stay hidden");
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
            assert!(!output.contains("query 1/3:"), "{output}");
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
        assert!(output.contains("\x1b[35mquery 1/3:\x1b[0m \x1b[1mcheckpoint\x1b[0m"));
        assert!(output.contains("\x1b[35mquery 2/3:\x1b[0m \x1b[1mworkspace\x1b[0m"));
        assert!(output.contains("\x1b[2m"), "separator rules are dim");
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
fn query_headings_keep_original_positions_and_sanitize_without_truncating() {
    let mut result = grouped_result();
    result.group_results[0].items.clear();
    let long_text = "中文e\u{301}👩‍💻".repeat(30);
    result.group_results[1].query =
        format!("pool  timeout\n\r\t\x1b[2J\u{2028}\u{2029}{long_text}");
    let expected = format!("pool  timeout    [2J  {long_text}");
    for color in [ColorMode::Auto, ColorMode::Never, ColorMode::Always] {
        for preview in [
            PreviewMode::None,
            PreviewMode::Short,
            PreviewMode::Full,
            PreviewMode::Detail,
        ] {
            let mut bytes = Vec::new();
            write_context_with_options(
                &mut bytes,
                &result,
                OutputOptions {
                    preview,
                    color,
                    ..OutputOptions::default()
                },
                false,
            )
            .expect("long query render");
            let output = String::from_utf8(bytes).expect("UTF-8");
            let heading = output
                .lines()
                .find(|line| line.contains("query 2/3:"))
                .expect("original query position");
            assert!(heading.contains(&expected), "{heading}");
            assert!(!output.contains("query 1/3:"));
            assert!(
                !output.contains("\x1b[2J"),
                "query must not clear the terminal"
            );
            if color != ColorMode::Always {
                assert!(
                    !output.contains('\x1b'),
                    "plain output must not contain ANSI escapes"
                );
            }
        }
    }
}

#[test]
fn single_query_does_not_add_a_divider() {
    let mut result = grouped_result();
    result.group_results.truncate(1);
    for preview in [
        PreviewMode::None,
        PreviewMode::Short,
        PreviewMode::Full,
        PreviewMode::Detail,
    ] {
        let mut bytes = Vec::new();
        write_context_with_options(
            &mut bytes,
            &result,
            OutputOptions {
                preview,
                color: ColorMode::Never,
                ..OutputOptions::default()
            },
            false,
        )
        .expect("single query render");
        let output = String::from_utf8(bytes).expect("UTF-8");
        assert!(output.contains("primary-source"));
        assert!(!output.contains("query 1/1:"), "{output}");
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
fn short_marks_truncated_source_lines_and_full_preserves_them() {
    for (content, short_line) in [
        ("a".repeat(160), "a".repeat(160)),
        ("a".repeat(161), format!("{}...", "a".repeat(160))),
        ("界😀".repeat(80), "界😀".repeat(80)),
        ("界😀".repeat(81), format!("{}...", "界😀".repeat(80))),
    ] {
        let mut result = grouped_result();
        result.group_results.clear();
        result.items[0].content = content.clone();
        for (preview, expected) in [
            (PreviewMode::Short, short_line.as_str()),
            (PreviewMode::Full, content.as_str()),
        ] {
            let mut bytes = Vec::new();
            write_context_with_options(
                &mut bytes,
                &result,
                OutputOptions {
                    preview,
                    color: ColorMode::Never,
                    ..OutputOptions::default()
                },
                false,
            )
            .expect("single-line render");
            let output = String::from_utf8(bytes).expect("UTF-8");
            assert_eq!(
                output.lines().find(|line| line.starts_with("  50: ")),
                Some(format!("  50: {expected}").as_str()),
                "{preview:?}: {output}"
            );
        }
    }
}

#[test]
fn bounded_previews_keep_fragment_text_after_restoring_a_long_line_prefix() {
    let fragment = format!("node.name,{}needle", "x".repeat(143));
    for prefix in [" ".repeat(200), "中😀".repeat(100)] {
        let line = format!("{prefix}{fragment}tail");
        for before in ["", "before\r\n"] {
            let matched_line = 50 + usize::from(!before.is_empty());
            let column_origin = if before.is_empty() { 4 } else { 0 };
            let mut result = grouped_result();
            result.group_results.clear();
            let item = &mut result.items[0];
            item.content = format!("{before}{line}");
            item.content_range = ContentRange::Text {
                start_line: 50,
                end_line: matched_line,
                start_byte_offset: 100,
                end_byte_offset: 100 + item.content.len(),
                start_byte_column: 4,
                end_byte_column: column_origin + line.len(),
            };
            item.range = item.content_range.clone();
            item.excerpt_range = Some(ContentRange::Text {
                start_line: matched_line,
                end_line: matched_line,
                start_byte_offset: 100 + before.len() + prefix.len(),
                end_byte_offset: 100 + before.len() + prefix.len() + fragment.len(),
                start_byte_column: column_origin + prefix.len(),
                end_byte_column: column_origin + prefix.len() + fragment.len(),
            });
            let original = result.clone();
            for preview in [PreviewMode::None, PreviewMode::Short, PreviewMode::Full] {
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
                    .expect("fragment preview");
                    let output = String::from_utf8(bytes).expect("UTF-8");
                    let label = if preview == PreviewMode::None {
                        format!("{matched_line}\t")
                    } else {
                        format!("  {matched_line}: ")
                    };
                    let displayed = output
                        .lines()
                        .find_map(|line| line.strip_prefix(&label))
                        .expect("matched source line");
                    assert!(displayed.contains(&fragment), "{preview:?}: {output}");
                    if preview == PreviewMode::Full {
                        assert_eq!(displayed, line);
                    } else {
                        assert!(displayed.starts_with("...") && displayed.ends_with("..."));
                        assert!(displayed.chars().count() <= 198, "bounded source context");
                    }
                    assert_eq!(
                        result, original,
                        "rendering preserves retrieval and source coordinates"
                    );
                }
            }
        }
    }
}

#[test]
fn minimal_preview_preserves_its_utf16_limit_without_a_restored_prefix() {
    for (content, expected) in [
        ("a".repeat(160), "a".repeat(160)),
        ("a".repeat(161), format!("{}...", "a".repeat(159))),
        (
            format!("{}😀", "a".repeat(158)),
            format!("{}😀", "a".repeat(158)),
        ),
        (
            format!("{}😀b", "a".repeat(158)),
            format!("{}...", "a".repeat(158)),
        ),
    ] {
        let mut result = grouped_result();
        result.group_results.clear();
        result.items[0].content = content;
        result.items[0].excerpt_range = None;
        let mut bytes = Vec::new();
        write_context_with_options(
            &mut bytes,
            &result,
            OutputOptions {
                preview: PreviewMode::None,
                color: ColorMode::Never,
                ..OutputOptions::default()
            },
            false,
        )
        .expect("minimal render");
        let output = String::from_utf8(bytes).expect("UTF-8");
        assert_eq!(
            output.lines().find_map(|line| line.strip_prefix("50\t")),
            Some(expected.as_str())
        );
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
