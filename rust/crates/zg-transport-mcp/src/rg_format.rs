//! Managed-rg presentation preserves all matches and requested context.

use std::collections::BTreeMap;
use zg_engine::api::context::{
    ContextResult,
    result::{ContentRange, ContextCoverage, ContextItem, EmptyReason, EntityMetadata},
};

pub(crate) fn format(reply: &ContextResult) -> String {
    let mut lines = Vec::new();
    let mut items: Vec<_> = reply.items.iter().collect();
    items.sort_by_key(|item| item.rank);
    let mut files: Vec<Vec<&ContextItem>> = Vec::new();
    for item in items {
        if let Some(group) = files
            .iter_mut()
            .find(|group| group[0].relative_path == item.relative_path)
        {
            group.push(item);
        } else {
            files.push(vec![item]);
        }
    }
    if files.is_empty() {
        lines.push(
            if reply.diagnostics.empty_reason == Some(EmptyReason::NoSearchableFiles) {
                "No searchable files."
            } else {
                "No matches."
            }
            .to_owned(),
        );
    }
    for file in files {
        lines.push(file[0].relative_path.display().to_string());
        let mut blocks: Vec<(Option<String>, Vec<&ContextItem>)> = Vec::new();
        for item in file {
            let label = symbol(item);
            if let Some((_, group)) = blocks.iter_mut().find(|(existing, group)| {
                label.is_some() && *existing == label && group[0].container == item.container
            }) {
                group.push(item);
            } else {
                blocks.push((label, vec![item]));
            }
        }
        for (label, group) in blocks {
            let item = group[0];
            let entries = source(&group);
            let redundant = group.len() == 1 && redundant_declaration(item, &entries);
            if let Some(label) = label.filter(|_| !redundant) {
                let range = &item.container.as_ref().expect("symbol container").range;
                let header = format!("  {} [{label}]", super::range_label(range));
                if entries.len() == 1 && group.len() == 1 {
                    lines.push(format!("{header} {}", entries[0].2));
                } else {
                    lines.push(header);
                    lines.extend(
                        entries
                            .into_iter()
                            .map(|(_, _, text)| format!("    {text}")),
                    );
                }
            } else {
                lines.extend(entries.into_iter().map(|(_, _, text)| format!("  {text}")));
            }
        }
    }
    if reply.coverage == ContextCoverage::RgTruncated {
        lines.push("\nMore matches were omitted by the explicit output bound. Remove or increase the trailing `head` bound to see them.".to_owned());
    }
    lines.join("\n")
}

fn symbol(item: &ContextItem) -> Option<String> {
    let container = item.container.as_ref()?;
    match container.metadata.as_ref().or(item.metadata.as_ref())? {
        EntityMetadata::Code(code) => {
            let name = code.symbol_name.as_ref()?;
            let qualified = code
                .scope
                .as_ref()
                .map_or_else(|| name.clone(), |scope| format!("{scope}.{name}"));
            let kind = serde_json::to_value(code.symbol_type?).ok()?;
            Some(format!("{} {qualified}", kind.as_str()?))
        }
        EntityMetadata::Markdown(markdown) => {
            let heading = markdown.heading.as_ref()?;
            Some(format!(
                "heading {}",
                markdown
                    .scope
                    .as_ref()
                    .map_or_else(|| heading.clone(), |scope| format!("{scope} > {heading}"))
            ))
        }
    }
}

fn source(items: &[&ContextItem]) -> Vec<(Option<usize>, bool, String)> {
    let mut numbered: BTreeMap<usize, (bool, String)> = BTreeMap::new();
    let mut unnumbered = Vec::new();
    for item in items {
        let matched = item.excerpt_range.as_ref().unwrap_or(&item.range);
        // A trailing newline is not an extra source line in rg's line-oriented output.
        for (offset, text) in item.content.lines().enumerate() {
            if let Some(first) = item.content_range.start_line() {
                let line = first + offset;
                let hit = matched.start_line().is_none_or(|first| line >= first)
                    && matched.last_line().is_none_or(|last| line <= last);
                numbered
                    .entry(line)
                    .and_modify(|(previous_hit, _)| *previous_hit |= hit)
                    .or_insert((hit, text.to_owned()));
            } else {
                unnumbered.push((None, false, format!("\t{text}")));
            }
        }
    }
    numbered
        .into_iter()
        .map(|(line, (hit, text))| {
            (
                Some(line),
                hit,
                format!("{line}{}\t{text}", if hit { ':' } else { '-' }),
            )
        })
        .chain(unnumbered)
        .collect()
}

fn redundant_declaration(item: &ContextItem, entries: &[(Option<usize>, bool, String)]) -> bool {
    let Some(container) = &item.container else {
        return false;
    };
    let Some(EntityMetadata::Code(code)) = container.metadata.as_ref().or(item.metadata.as_ref())
    else {
        return false;
    };
    let Some(name) = &code.symbol_name else {
        return false;
    };
    let hits: Vec<_> = entries.iter().filter(|(_, hit, _)| *hit).collect();
    hits.len() == 1
        && matches!(container.range, ContentRange::Text { .. })
        && hits[0].0 == container.range.start_line()
        && hits[0].2.contains(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captured_node_rg_presentation() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../../../compat/mcp/rg-presentation.json"))
                .expect("fixtures");
        for case in fixtures["cases"].as_array().expect("cases") {
            let result: ContextResult =
                serde_json::from_value(case["result"].clone()).expect("context");
            assert_eq!(
                format(&result),
                case["expected"].as_str().expect("expected"),
                "{}",
                case["id"]
            );
        }
    }

    #[test]
    fn preserves_long_lines_all_context_and_explicit_truncation() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../compat/mcp/search-presentation.json");
        let mut result: ContextResult = serde_json::from_value(
            zg_testkit::load_mcp_search_cases(&path)
                .expect("fixtures")
                .remove(0)
                .result,
        )
        .expect("result");
        result.items.truncate(1);
        result.items[0].container = None;
        result.items[0].content = (1..=20)
            .map(|line| format!("line-{line}-{}", "x".repeat(200)))
            .collect::<Vec<_>>()
            .join("\n");
        result.coverage = ContextCoverage::RgTruncated;
        let text = format(&result);
        assert!(text.starts_with("src/sample.ts\n"));
        assert!(text.contains(&format!("line-20-{}", "x".repeat(200))));
        assert!(text.contains("More matches were omitted"));
        result.coverage = ContextCoverage::RgExhaustive;
        assert!(!format(&result).contains("More matches were omitted"));
    }
}
