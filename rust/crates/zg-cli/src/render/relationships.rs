use std::io::{self, Write};

use zg_engine::api::relationships::{RelationshipDirection, RelationshipResult, SymbolDefinition};

use crate::{
    ColorMode, output_path,
    theme::{StatusTheme, clean},
};

/// Writes relationship matches and suggestions in the shared CLI/MCP text format.
/// Text coordinates are one-based; structured API columns remain zero-based.
/// # Errors
/// Returns the underlying writer error.
pub fn write_relationships(
    mut writer: impl Write,
    result: &RelationshipResult,
    direction: RelationshipDirection,
    color: ColorMode,
    terminal: bool,
) -> io::Result<()> {
    let theme = StatusTheme::new(color, terminal);
    let label = match direction {
        RelationshipDirection::Callers => "Callers",
        RelationshipDirection::Callees => "Callees",
    };
    if result.matches.is_empty() {
        writeln!(writer, "No matches.")?;
    }
    for (index, matched) in result.matches.iter().enumerate() {
        if index > 0 {
            writeln!(writer)?;
        }
        writeln!(writer, "{}", definition(&matched.symbol, theme))?;
        writeln!(writer, "  {}:", theme.metadata_label(label))?;
        if matched.edges.is_empty() {
            writeln!(writer, "    No resolved call sites.")?;
        }
        for edge in &matched.edges {
            let endpoint = edge.symbol.as_ref().map_or_else(
                || "(symbol unavailable)".to_owned(),
                |symbol| definition(symbol, theme),
            );
            writeln!(writer, "    {endpoint}")?;
            let path = clean(&output_path(&edge.file_path).display().to_string());
            let location = match (edge.line, edge.column) {
                (Some(line), Some(column)) => format!("{path}:{line}:{}", column.saturating_add(1)),
                (Some(line), None) => format!("{path}:{line}"),
                (None, _) => format!("{path} (position unavailable)"),
            };
            writeln!(
                writer,
                "      {} {}",
                theme.label("call at"),
                theme.path(&location)
            )?;
        }
        if matched.truncated {
            writeln!(
                writer,
                "    {}",
                theme.warning("... more call sites omitted")
            )?;
        }
    }
    if !result.suggestions.is_empty() {
        writeln!(
            writer,
            "\n{}",
            theme.metadata_label("Suggestions (retry with an exact name):")
        )?;
        for symbol in &result.suggestions {
            writeln!(writer, "  {}", definition(symbol, theme))?;
        }
    }
    Ok(())
}

fn definition(symbol: &SymbolDefinition, theme: StatusTheme) -> String {
    let location = format!(
        "{}:{}",
        output_path(&symbol.file_path).display(),
        symbol.start_line
    );
    format!("{}  {}", clean(&symbol.name), theme.path(&clean(&location)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zg_engine::api::relationships::{CallSite, SymbolRelationships};

    fn symbol(name: &str, path: &str) -> SymbolDefinition {
        SymbolDefinition {
            name: name.into(),
            file_path: path.into(),
            start_line: 1,
            end_line: 10,
        }
    }

    fn render(result: &RelationshipResult, direction: RelationshipDirection) -> String {
        let mut output = Vec::new();
        write_relationships(&mut output, result, direction, ColorMode::Never, false)
            .expect("render relationships");
        String::from_utf8(output).expect("UTF-8 output")
    }

    #[test]
    fn definitions_and_call_sites_stay_distinct_and_repeated_calls_remain_visible() {
        let mut result = RelationshipResult {
            matches: vec![SymbolRelationships {
                symbol: symbol("run", "main.rs"),
                truncated: true,
                edges: vec![
                    CallSite {
                        file_path: "main.rs".into(),
                        symbol: Some(symbol("send", "net.rs")),
                        line: Some(3),
                        column: Some(0),
                    },
                    CallSite {
                        file_path: "main.rs".into(),
                        symbol: Some(symbol("send", "net.rs")),
                        line: Some(7),
                        column: None,
                    },
                    CallSite {
                        file_path: "startup.rs".into(),
                        symbol: None,
                        line: None,
                        column: None,
                    },
                ],
            }],
            suggestions: Vec::new(),
        };
        assert_eq!(
            render(&result, RelationshipDirection::Callees),
            concat!(
                "run  main.rs:1\n  Callees:\n",
                "    send  net.rs:1\n      call at main.rs:3:1\n",
                "    send  net.rs:1\n      call at main.rs:7\n",
                "    (symbol unavailable)\n      call at startup.rs (position unavailable)\n",
                "    ... more call sites omitted\n",
            )
        );
        result.matches.push(SymbolRelationships {
            symbol: symbol("run", "other.rs"),
            truncated: false,
            edges: Vec::new(),
        });
        let text = render(&result, RelationshipDirection::Callers);
        assert!(text.contains("\nrun  other.rs:1\n  Callers:\n    No resolved call sites.\n"));
    }

    #[test]
    fn suggestions_are_not_presented_as_relationship_matches() {
        let mut result = RelationshipResult::default();
        assert_eq!(
            render(&result, RelationshipDirection::Callers),
            "No matches.\n"
        );
        result.suggestions.push(symbol("run\n", "main.rs"));
        assert_eq!(
            render(&result, RelationshipDirection::Callers),
            concat!(
                "No matches.\n\nSuggestions (retry with an exact name):\n",
                "  run   main.rs:1\n",
            )
        );
    }
}
