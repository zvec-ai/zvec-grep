//! Public indexed-search presentation, independent of retrieval and ranking.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zg_engine::api::context::{
    ContextResult,
    result::{
        ContentRange, ContextItem, ContextItemKind, ContextItemStatus, ContextQueryGroupRole,
        ContextSelectionReason, EmptyReason, EntityMetadata,
    },
};

use super::{matched_by_label, range_label};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum SearchPreview {
    #[default]
    Short,
    Full,
}

pub(crate) fn format_search_result(reply: &ContextResult, preview: SearchPreview) -> String {
    let legacy_current_index = reply.freshness.as_deref() == Some("served_from_current_index");
    // Reading an existing index describes provenance, not verified freshness.
    // Preserve uncertainty from older providers even when no returned hit is stale.
    let stale = legacy_current_index
        || reply.freshness.as_deref() == Some("possibly_stale")
        || reply
            .items
            .iter()
            .any(|item| item.status == ContextItemStatus::PossiblyStale);
    let mut lines = vec![format!(
        "freshness: {}",
        if stale { "possibly_stale" } else { "fresh" }
    )];
    let refresh = reply
        .background_refresh
        .as_deref()
        .filter(|refresh| stale || !matches!(*refresh, "off" | "idle"));
    if legacy_current_index || refresh.is_some() {
        lines.push("results: served_from_current_index".to_owned());
    }
    if let Some(refresh) = refresh {
        lines.push(format!("background_refresh: {refresh}"));
    }
    if reply.items.is_empty() {
        lines.push(
            match reply.diagnostics.empty_reason {
                Some(EmptyReason::NoSearchableFiles) => "No searchable files.",
                _ => "No matches.",
            }
            .to_owned(),
        );
        return lines.join("\n");
    }

    if let Some(index) = &reply.diagnostics.index
        && index.query_groups.len() > 1
    {
        lines.push(format!("query groups ({}):", index.query_groups.len()));
        lines.extend(index.query_groups.iter().map(|group| {
            format!(
                "  {} [{}]: {}",
                group.id,
                group_role(group.role),
                group.query
            )
        }));
        lines.push("selection: primary-group coverage then global_fill; prioritized<=6; all candidates detailed".to_owned());
        lines.push(String::new());
    }

    let mut items: Vec<_> = reply.items.iter().collect();
    items.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then_with(|| left.range.start_line().cmp(&right.range.start_line()))
            .then_with(|| range_label(&left.range).cmp(&range_label(&right.range)))
    });
    for (position, item) in items.into_iter().enumerate() {
        if position > 0 {
            lines.push(String::new());
        }
        append_item(&mut lines, item, preview);
    }
    lines.join("\n")
}

fn append_item(lines: &mut Vec<String>, item: &ContextItem, preview: SearchPreview) {
    let selection = match item.selection_reason {
        Some(ContextSelectionReason::Coverage) => format!(
            " [group_coverage: {}]",
            item.coverage_group.as_deref().unwrap_or("unknown")
        ),
        Some(ContextSelectionReason::GlobalFill) => " [global_fill]".to_owned(),
        None => String::new(),
    };
    let header_range = item.container.as_ref().map_or(&item.range, |c| &c.range);
    lines.push(format!(
        "#{}{selection} matchedBy={} {}:{}",
        item.rank,
        matched_by_label(item.matched_by),
        item.relative_path.display(),
        range_label(header_range)
    ));
    if !item.query_groups.is_empty() {
        lines.push(format!(
            "groups: {}",
            item.query_groups
                .iter()
                .map(|group| format!(
                    "{}#{} ({})",
                    group.id,
                    group.rank,
                    matched_by_label(group.matched_by)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if item.status == ContextItemStatus::PossiblyStale {
        lines.push("status: possibly_stale".to_owned());
    }
    let outline = outline_lines(item, preview);
    let source = source_lines(item, preview);
    append_metadata(lines, item, &outline, &source);
    if !outline.is_empty() {
        lines.push("outline:".to_owned());
        lines.extend(outline);
    }
    if item.kind != ContextItemKind::LexicalMatch
        && let Some(excerpt) = &item.excerpt_range
        && range_label(excerpt) != range_label(&item.range)
    {
        lines.push(format!("matched: {}", range_label(excerpt)));
    }
    if !source.is_empty() {
        if item.kind != ContextItemKind::LexicalMatch {
            lines.push("source:".to_owned());
        }
        lines.extend(source);
    }
}

fn append_metadata(
    lines: &mut Vec<String>,
    item: &ContextItem,
    outline: &[String],
    source: &[String],
) {
    match &item.metadata {
        Some(EntityMetadata::Code(code)) => {
            if let Some(name) = &code.symbol_name
                && ((item.kind == ContextItemKind::LexicalMatch && item.container.is_some())
                    || !outline.iter().chain(source).any(|line| line.contains(name)))
            {
                let kind = code.symbol_type.map_or(String::new(), |kind| {
                    // SymbolType serializes to the public lowercase spelling.
                    let value = serde_json::to_value(kind).unwrap_or_default();
                    format!("{} ", value.as_str().unwrap_or_default())
                });
                let scope = code
                    .scope
                    .as_ref()
                    .map_or(String::new(), |s| format!(" scope: {s}"));
                lines.push(format!("symbol: {kind}{name}{scope}"));
            }
        }
        Some(EntityMetadata::Markdown(markdown)) => {
            if let Some(heading) = &markdown.heading {
                lines.push(format!("heading: {heading}"));
            }
            if let Some(level) = markdown.level {
                lines.push(format!("heading_level: {level}"));
            }
            if let Some(scope) = &markdown.scope {
                lines.push(format!("scope: {scope}"));
            }
        }
        None => {}
    }
}

fn outline_lines(item: &ContextItem, preview: SearchPreview) -> Vec<String> {
    let Some(outline) = item.outline.as_deref().filter(|value| !value.is_empty()) else {
        return Vec::new();
    };
    let mut lines: Vec<_> = split_lines(outline).map(str::to_owned).collect();
    if preview == SearchPreview::Short && lines.len() > 7 {
        lines.truncate(7);
        lines.push("...".to_owned());
    }
    lines
}

fn source_lines(item: &ContextItem, preview: SearchPreview) -> Vec<String> {
    if item.content.is_empty() {
        return Vec::new();
    }
    let content: Vec<_> = split_lines(&item.content).collect();
    let first = item.content_range.start_line();
    let (start, end) = if preview == SearchPreview::Short && content.len() > 10 {
        let anchor = item
            .excerpt_range
            .as_ref()
            .and_then(ContentRange::start_line)
            .map_or(0, |line| {
                first.map_or(0, |first| line.saturating_sub(first))
            });
        let anchor = if anchor < content.len() { anchor } else { 0 };
        let before = if item
            .excerpt_range
            .as_ref()
            .is_some_and(|range| range.line_count().is_some_and(|count| count >= 10))
        {
            0
        } else {
            2
        };
        let start = anchor.saturating_sub(before).min(content.len() - 10);
        (start, start + 10)
    } else {
        (0, content.len())
    };
    let mut lines = Vec::new();
    if start > 0 {
        lines.push("...".to_owned());
    }
    for (offset, line) in content.iter().enumerate().take(end).skip(start) {
        let number = first.map(|first| first + offset);
        let prefix = number.map_or(String::new(), |number| number.to_string());
        let marker = if item.kind == ContextItemKind::LexicalMatch && number.is_some() {
            let range = item.excerpt_range.as_ref().unwrap_or(&item.range);
            let matched = range.start_line().is_none_or(|start| {
                number.is_some_and(|number| {
                    number >= start && number < start + range.line_count().unwrap_or(1)
                })
            });
            if matched { ":" } else { "-" }
        } else {
            ""
        };
        let text = if preview == SearchPreview::Full {
            (*line).to_owned()
        } else {
            truncate_source(line)
        };
        lines.push(format!("{prefix}{marker}\t{text}"));
    }
    if end < content.len() {
        lines.push("...".to_owned());
    }
    lines
}

fn split_lines(content: &str) -> impl Iterator<Item = &str> {
    // Match the Node.js splitter, including a final empty source line.
    content
        .split_inclusive('\n')
        .map(|line| {
            line.strip_suffix('\n')
                .map_or(line, |line| line.strip_suffix('\r').unwrap_or(line))
        })
        .chain((content.is_empty() || content.ends_with('\n')).then_some(""))
}

fn truncate_source(line: &str) -> String {
    if line.encode_utf16().count() <= 160 {
        return line.to_owned();
    }
    let mut units = 0;
    let prefix: String = line
        .chars()
        .take_while(|character| {
            units += character.len_utf16();
            units <= 159
        })
        .collect();
    format!("{prefix}...")
}

const fn group_role(role: ContextQueryGroupRole) -> &'static str {
    match role {
        ContextQueryGroupRole::Primary => "primary",
        ContextQueryGroupRole::Supplemental => "supplemental",
    }
}

#[cfg(test)]
mod tests;
