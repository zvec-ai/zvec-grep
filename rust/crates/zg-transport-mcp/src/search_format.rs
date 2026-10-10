//! Public indexed-search presentation, independent of retrieval and ranking.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zg_engine::api::context::{
    ContextResult,
    result::{
        ContentPreview, ContentRange, ContextItem, ContextItemKind, ContextItemStatus,
        ContextQueryGroupRole, ContextSelectionReason, EmptyReason, EntityMetadata,
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
    append_target_diagnostics(&mut lines, reply);
    if reply.items.is_empty() {
        lines.push(
            match reply.diagnostics.empty_reason {
                Some(EmptyReason::NoSearchableFiles) if reply.diagnostics.index.is_some() => {
                    "Selected index tables are empty."
                }
                Some(EmptyReason::NoSearchableFiles) => "No searchable files.",
                Some(EmptyReason::NoSupportedTargets) => {
                    "No enabled index table supports this query."
                }
                _ => "No matches.",
            }
            .to_owned(),
        );
        return lines.join("\n");
    }
    if reply.items.iter().any(|item| item.content_ref.is_some()) {
        lines.push("Use zvec_grep_read_content with the workspace root and a result's reference to retrieve its full indexed content.".into());
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

    for (position, item) in reply.items.iter().enumerate() {
        if let Some(group) = reply.diagnostics.index.as_ref().and_then(|index| {
            index
                .result_groups
                .iter()
                .find(|group| group.item_start == position)
        }) {
            lines.push(format!(
                "result group: {} kinds={} scoring={:?} merge={}; ranked within this group",
                group.id,
                group
                    .kinds
                    .iter()
                    .map(|kind| kind.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                group.scoring,
                if group.kinds.len() > 1 {
                    "compatible_vector_scores"
                } else {
                    "independent_kind"
                }
            ));
        }
        if position > 0 {
            lines.push(String::new());
        }
        append_item(&mut lines, item, preview);
    }
    lines.join("\n")
}

fn append_target_diagnostics(lines: &mut Vec<String>, reply: &ContextResult) {
    if let Some(index) = &reply.diagnostics.index {
        if index.incomplete {
            lines.push("incomplete: one or more target searches failed".into());
        }
        for target in &index.targets {
            lines.push(format!(
                "route: input={} target={} model={} status={:?}{}",
                index.input_kind.as_str(),
                target.kind.as_str(),
                target.model_ref,
                target.status,
                target
                    .reason
                    .as_ref()
                    .map_or_else(String::new, |reason| format!(" ({reason})")),
            ));
        }
    }
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
    if let ContentPreview::Image { format, size_bytes } = &item.preview {
        lines.push(format!(
            "#{}{selection} matchedBy={} {}",
            item.rank,
            matched_by_label(item.matched_by),
            item.relative_path.display()
        ));
        lines.push(format!(
            "type: image; format: {}; size: {} bytes",
            format.as_str(),
            size_bytes
        ));
        append_content_reference(lines, item);
        if item.status == ContextItemStatus::PossiblyStale {
            lines.push("status: possibly_stale".into());
        }
        return;
    }
    let header_range = item.container.as_ref().map_or(&item.range, |c| &c.range);
    lines.push(format!(
        "#{}{selection} matchedBy={} {}:{}",
        item.rank,
        matched_by_label(item.matched_by),
        item.relative_path.display(),
        range_label(header_range)
    ));
    if item.content_ref.is_some() {
        lines.push(format!("type: {}", item.preview.kind().as_str()));
    }
    append_content_reference(lines, item);
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

fn append_content_reference(lines: &mut Vec<String>, item: &ContextItem) {
    if let Some(reference) = &item.content_ref {
        let value = serde_json::json!({
            "generation": reference.generation,
            "entityId": reference.entity_id,
        });
        lines.push(format!("reference: {value}"));
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
    let text = item.preview.text().unwrap_or_default();
    if text.is_empty() {
        return Vec::new();
    }
    let content: Vec<_> = split_lines(text).collect();
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
pub(crate) mod tests;
