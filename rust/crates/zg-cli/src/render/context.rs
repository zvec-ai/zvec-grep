//! Indexed CLI presentation. Preview selection never changes retrieval.

use std::{
    collections::BTreeSet,
    io::{self, Write},
};

use unicode_width::UnicodeWidthStr;

use zg_engine::api::context::{
    ContextResult,
    result::{
        ContentRange, ContextCoverage, ContextItem, ContextItemKind, ContextItemStatus,
        EmptyReason, EntityMetadata,
    },
};

use crate::{
    OutputOptions, PreviewMode,
    highlight::QueryHighlighter,
    output_path,
    preview::{source_preview, split_lines, truncate_utf16 as truncate},
    theme::StatusTheme,
};

pub(super) fn write(
    writer: &mut impl Write,
    result: &ContextResult,
    options: OutputOptions,
    terminal: bool,
) -> io::Result<()> {
    let theme = StatusTheme::new(options.color, terminal);
    if options.preview == PreviewMode::Detail {
        return write_detail(writer, result, options, theme);
    }
    if result.group_results.is_empty() {
        if result.items.is_empty()
            && result.diagnostics.empty_reason == Some(EmptyReason::NoSearchableFiles)
        {
            return writeln!(writer, "No searchable files.");
        }
        return write_items(writer, &result.items, &result.query, options, theme);
    }
    let groups = result
        .group_results
        .iter()
        .enumerate()
        .filter(|(_, group)| !group.items.is_empty())
        .collect::<Vec<_>>();
    if groups.is_empty() {
        return writeln!(writer, "No matches.");
    }
    for (position, (index, group)) in groups.into_iter().enumerate() {
        if position > 0 {
            writeln!(writer)?;
        }
        if result.group_results.len() > 1 {
            writeln!(
                writer,
                "{}\n",
                query_divider(
                    &group.query,
                    index,
                    result.group_results.len(),
                    theme,
                    crate::theme::columns(),
                )
            )?;
        }
        write_items(writer, &group.items, &group.query, options, theme)?;
    }
    Ok(())
}

fn query_divider(
    query: &str,
    index: usize,
    total: usize,
    theme: StatusTheme,
    columns: usize,
) -> String {
    // Keep the original query (including spaces), but never let it add output
    // lines or terminal control sequences. Long queries may wrap visually.
    let query = crate::theme::clean(query).replace(['\u{2028}', '\u{2029}'], " ");
    let label = format!("query {}/{total}:", index + 1);
    if !theme.color {
        return format!("-- {label} {query} --");
    }
    let rule = if crate::theme::unicode_supported() {
        "─"
    } else {
        "-"
    };
    let used = 3 + label.width() + 1 + query.width();
    let remaining = columns.saturating_sub(used);
    let tail = if remaining > 1 {
        format!(" {}", theme.muted(&rule.repeat(remaining - 1)))
    } else {
        String::new()
    };
    format!(
        "{} {} \x1b[1m{query}\x1b[0m{tail}",
        theme.muted(&rule.repeat(2)),
        theme.metadata_label(&label),
    )
}

fn write_items(
    writer: &mut impl Write,
    items: &[ContextItem],
    query: &str,
    options: OutputOptions,
    theme: StatusTheme,
) -> io::Result<()> {
    if matches!(options.preview, PreviewMode::Short | PreviewMode::Full) {
        return super::compact::write_items(writer, items, query, options, theme);
    }
    if items.is_empty() {
        return writeln!(writer, "No matches.");
    }
    let highlighter = QueryHighlighter::new(query, theme.color);
    for (position, item) in sorted(items.iter()).into_iter().enumerate() {
        if position > 0 {
            writeln!(writer)?;
        }
        let score = if options.trace {
            item.score.map_or(String::new(), |value| {
                format!(" {}{}", theme.metadata_label("score="), score(value))
            })
        } else {
            String::new()
        };
        let range = item
            .container
            .as_ref()
            .map_or(&item.range, |container| &container.range);
        writeln!(
            writer,
            "#{}{score} {}:{}",
            item.rank,
            output_path(&item.relative_path).display(),
            range_label(range)
        )?;
        write_item_preview(writer, item, options, &highlighter, theme)?;
        if options.trace
            && let Some(trace) = trace(item)
        {
            writeln!(writer, "{} {trace}", theme.metadata_label("trace:"))?;
        }
    }
    Ok(())
}

pub(super) fn write_item_preview(
    writer: &mut impl Write,
    item: &ContextItem,
    options: OutputOptions,
    highlighter: &QueryHighlighter,
    theme: StatusTheme,
) -> io::Result<()> {
    if item.status == ContextItemStatus::PossiblyStale {
        writeln!(writer, "{} possibly_stale", theme.metadata_label("status:"))?;
    }
    let outline = outline_lines(item, options.preview);
    let source = source_lines(item, options.preview, highlighter);
    match &item.metadata {
        Some(EntityMetadata::Code(code)) => {
            if let Some(name) = &code.symbol_name
                && !outline
                    .iter()
                    .chain(&source)
                    .any(|line| line.contains(name))
            {
                let kind = code
                    .symbol_type
                    .map_or(String::new(), |kind| format!("[{}] ", enum_label(kind)));
                let scope = code.scope.as_ref().map_or(String::new(), |scope| {
                    format!(" {} {scope}", theme.metadata_label("scope:"))
                });
                writeln!(
                    writer,
                    "{} {kind}{name}{scope}",
                    theme.metadata_label("symbol:")
                )?;
            }
        }
        Some(EntityMetadata::Markdown(markdown)) => {
            if let Some(heading) = &markdown.heading {
                writeln!(writer, "{} {heading}", theme.metadata_label("heading:"))?;
            }
            if let Some(level) = markdown.level {
                writeln!(writer, "{} {level}", theme.metadata_label("heading_level:"))?;
            }
            if let Some(scope) = &markdown.scope {
                writeln!(writer, "{} {scope}", theme.metadata_label("scope:"))?;
            }
        }
        None => {}
    }
    if !outline.is_empty() {
        writeln!(writer, "{}", theme.metadata_label("outline:"))?;
        for line in outline {
            writeln!(writer, "{line}")?;
        }
    }
    if options.preview != PreviewMode::None
        && let Some(range) = matched_range(item)
    {
        writeln!(
            writer,
            "{} {}",
            theme.metadata_label("matched:"),
            range_label(range)
        )?;
    }
    if !source.is_empty() && options.preview != PreviewMode::None {
        writeln!(writer, "{}", theme.metadata_label("source:"))?;
    }
    for line in source {
        writeln!(writer, "{line}")?;
    }
    Ok(())
}

fn write_detail(
    writer: &mut impl Write,
    result: &ContextResult,
    options: OutputOptions,
    theme: StatusTheme,
) -> io::Result<()> {
    let grouped = !result.group_results.is_empty();
    let items: Vec<_> = if grouped {
        result
            .group_results
            .iter()
            .flat_map(|group| &group.items)
            .collect()
    } else {
        result.items.iter().collect()
    };
    let highlighter = QueryHighlighter::new(&result.query, theme.color);
    let context = if result.workspace_index.is_some() {
        "workspace index"
    } else {
        "index"
    };
    field(
        writer,
        theme,
        "Context",
        &format!("{} {context}", output_path(&result.root).display()),
    )?;
    let coverage = match result.coverage {
        ContextCoverage::RankedSample => "ranked_sample",
        ContextCoverage::RgExhaustive => "rg_exhaustive",
        ContextCoverage::RgTruncated => "rg_truncated",
    };
    field(writer, theme, "Coverage", coverage)?;
    field(
        writer,
        theme,
        "Files",
        &items
            .iter()
            .map(|item| &item.relative_path)
            .collect::<BTreeSet<_>>()
            .len()
            .to_string(),
    )?;
    if items.is_empty() {
        let reason = if result.diagnostics.empty_reason == Some(EmptyReason::NoSearchableFiles) {
            "No searchable files"
        } else {
            "No matches"
        };
        field(writer, theme, "Reason", reason)?;
    } else if grouped {
        for (index, group) in result.group_results.iter().enumerate() {
            if group.items.is_empty() {
                continue;
            }
            if result.group_results.len() > 1 {
                writeln!(
                    writer,
                    "\n{}",
                    query_divider(
                        &group.query,
                        index,
                        result.group_results.len(),
                        theme,
                        crate::theme::columns(),
                    )
                )?;
            }
            let highlighter = QueryHighlighter::new(&group.query, theme.color);
            write_detail_items(writer, &group.items, options, theme, &highlighter)?;
        }
    } else {
        write_detail_items(writer, &result.items, options, theme, &highlighter)?;
    }
    Ok(())
}

fn write_detail_items(
    writer: &mut impl Write,
    items: &[ContextItem],
    options: OutputOptions,
    theme: StatusTheme,
    highlighter: &QueryHighlighter,
) -> io::Result<()> {
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
    files.sort_by_key(|group| {
        (
            group.iter().map(|item| item.rank).min().unwrap_or(0),
            &group[0].relative_path,
        )
    });
    for group in files {
        writeln!(writer)?;
        field(
            writer,
            theme,
            "File",
            &theme.path(&output_path(&group[0].relative_path).display().to_string()),
        )?;
        for item in sorted(group) {
            let score = item.score.map_or(String::new(), |value| {
                format!(" {}{}", theme.metadata_label("score="), score(value))
            });
            writeln!(
                writer,
                "\n  {} {}{score}",
                StatusTheme::accent(&format!("#{}", item.rank)),
                summary(item)
            )?;
            let status = match item.status {
                ContextItemStatus::Fresh => "fresh",
                ContextItemStatus::PossiblyStale => "possibly_stale",
            };
            writeln!(
                writer,
                "  {} {}  {} {status}",
                theme.metadata_label("Range:"),
                range_label(&item.range),
                theme.metadata_label("Status:")
            )?;
            if let Some(range) = matched_range(item) {
                writeln!(
                    writer,
                    "  {} {}",
                    theme.metadata_label("Matched:"),
                    StatusTheme::accent(&range_label(range))
                )?;
            }
            for line in metadata_lines(item, theme) {
                writeln!(writer, "  {line}")?;
            }
            for (label, lines) in [
                ("Outline", outline_lines(item, PreviewMode::Detail)),
                (
                    "Source",
                    source_lines(item, PreviewMode::Detail, highlighter),
                ),
            ] {
                if !lines.is_empty() {
                    writeln!(writer, "  {}", theme.metadata_label(&format!("{label}:")))?;
                    for line in lines {
                        writeln!(writer, "    {line}")?;
                    }
                }
            }
            if options.trace
                && let Some(trace) = trace(item)
            {
                writeln!(writer, "  {} {trace}", theme.metadata_label("Trace:"))?;
            }
        }
    }
    Ok(())
}

fn metadata_lines(item: &ContextItem, theme: StatusTheme) -> Vec<String> {
    let mut lines = Vec::new();
    match &item.metadata {
        Some(EntityMetadata::Code(code)) => {
            let kind = code.symbol_type.map_or_else(
                || "code".into(),
                |kind| format!("code/{}", enum_label(kind)),
            );
            lines.push(format!("{} {kind}", theme.metadata_label("Kind:")));
            if let Some(name) = &code.symbol_name {
                lines.push(format!("{} {name}", theme.metadata_label("Symbol:")));
            }
            if let Some(scope) = &code.scope {
                lines.push(format!("{} {scope}", theme.metadata_label("Scope:")));
            }
            if let Some(signature) = &code.signature {
                lines.push(format!(
                    "{} {}",
                    theme.metadata_label("Signature:"),
                    truncate(&one_line(signature), 120)
                ));
            }
        }
        Some(EntityMetadata::Markdown(markdown)) => {
            lines.push(format!("{} markdown", theme.metadata_label("Kind:")));
            if let Some(heading) = &markdown.heading {
                lines.push(format!("{} {heading}", theme.metadata_label("Heading:")));
            }
            if let Some(level) = markdown.level {
                lines.push(format!(
                    "{} {level}",
                    theme.metadata_label("Heading level:")
                ));
            }
            if let Some(scope) = &markdown.scope {
                lines.push(format!("{} {scope}", theme.metadata_label("Scope:")));
            }
        }
        None => {}
    }
    lines
}

fn summary(item: &ContextItem) -> String {
    match &item.metadata {
        Some(EntityMetadata::Code(code)) => {
            let kind = code.symbol_type.map_or_else(|| "code".into(), enum_label);
            code.symbol_name
                .as_ref()
                .map_or_else(|| kind.clone(), |name| format!("{kind} {name}"))
        }
        Some(EntityMetadata::Markdown(markdown)) => markdown
            .heading
            .as_ref()
            .map_or_else(|| "markdown".into(), |heading| format!("heading {heading}")),
        None => enum_label(item.kind),
    }
}

fn outline_lines(item: &ContextItem, preview: PreviewMode) -> Vec<String> {
    if preview == PreviewMode::None {
        return Vec::new();
    }
    let Some(outline) = item
        .outline
        .as_deref()
        .filter(|outline| !outline.is_empty())
    else {
        return Vec::new();
    };
    split_lines(outline).map(str::to_owned).collect()
}

fn source_lines(
    item: &ContextItem,
    preview: PreviewMode,
    highlighter: &QueryHighlighter,
) -> Vec<String> {
    let source = source_preview(item, (preview == PreviewMode::None).then_some(1));
    let first = item.content_range.start_line();
    source
        .window
        .map(|offset| {
            let prefix = first.map_or(String::new(), |first| (first + offset).to_string());
            let line = source.lines[offset];
            let text = if preview == PreviewMode::None {
                truncate(line, 160)
            } else {
                line.to_owned()
            };
            format!("{prefix}\t{}", highlighter.highlight(&text))
        })
        .collect()
}

fn matched_range(item: &ContextItem) -> Option<&ContentRange> {
    item.excerpt_range.as_ref().filter(|range| {
        item.kind != ContextItemKind::LexicalMatch && range_label(range) != range_label(&item.range)
    })
}

fn sorted<'a>(items: impl IntoIterator<Item = &'a ContextItem>) -> Vec<&'a ContextItem> {
    let mut items: Vec<_> = items.into_iter().collect();
    items.sort_by_key(|item| (item.rank, item.range.start_line(), range_label(&item.range)));
    items
}

fn field(writer: &mut impl Write, theme: StatusTheme, label: &str, value: &str) -> io::Result<()> {
    writeln!(
        writer,
        "{} {value}",
        theme.metadata_label(&format!("{label}:"))
    )
}
fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn score(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.4}")
    }
}
fn enum_label(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}
fn range_label(range: &ContentRange) -> String {
    match (range.start_line(), range.last_line()) {
        (Some(first), Some(last)) if first == last => first.to_string(),
        (Some(first), Some(last)) => format!("{first}-{last}"),
        _ => match range {
            ContentRange::Byte {
                start_offset,
                end_offset,
            } => format!("bytes:{start_offset}-{end_offset}"),
            _ => "file".into(),
        },
    }
}

fn trace(item: &ContextItem) -> Option<String> {
    let trace = item.trace.as_ref()?;
    let mut queries: Vec<(&str, Vec<String>)> = Vec::new();
    for recall in &trace.recall {
        let query = if recall.query.trim().is_empty() {
            "(unknown query)"
        } else {
            recall.query.trim()
        };
        let path = enum_label(recall.path);
        let route = if recall.route_id.is_empty() || recall.route_id == path {
            path
        } else {
            format!("{}/{path}", recall.route_id)
        };
        let route = if recall.found {
            format!(
                "{route} #{}{}",
                recall
                    .rank
                    .map_or_else(|| "n/a".into(), |rank| rank.to_string()),
                if recall.forced { " forced" } else { "" }
            )
        } else {
            format!("{route} missing")
        };
        if let Some((_, routes)) = queries.iter_mut().find(|(name, _)| *name == query) {
            routes.push(route);
        } else {
            queries.push((query, vec![route]));
        }
    }
    let mut parts: Vec<_> = queries
        .into_iter()
        .map(|(query, routes)| {
            format!(
                "query {}: {}",
                serde_json::to_string(query).expect("string serialization"),
                routes.join(", ")
            )
        })
        .collect();
    parts.push(format!(
        "fused #{}{}",
        trace.fusion.rank,
        if trace.fusion.forced { " forced" } else { "" }
    ));
    if let Some(ranking) = &trace.ranking {
        parts.push(format!("reranked #{}", ranking.rank));
    }
    Some(parts.join("; "))
}

#[cfg(test)]
mod tests {
    use super::query_divider;
    use crate::theme::StatusTheme;
    use unicode_width::UnicodeWidthStr;

    fn without_sgr(value: &str) -> String {
        value
            .split("\x1b[")
            .enumerate()
            .map(|(index, part)| {
                if index == 0 {
                    part
                } else {
                    part.split_once('m').expect("SGR terminator").1
                }
            })
            .collect()
    }

    #[test]
    fn query_dividers_respect_display_width_at_terminal_boundaries() {
        let theme = StatusTheme { color: true };
        for terminal_columns in [20, 40, 80, 120] {
            // Production reserves the last column to avoid an automatic wrap.
            let available = terminal_columns - 1;
            let query_budget = available - "-- query 1/2: ".width();
            for seed in ["a", "中", "e\u{301}", "👩‍💻"] {
                for (query_width, expected_width) in [
                    (query_budget - 3, available),
                    (query_budget - 1, available - 1),
                    (query_budget, available),
                    (query_budget + 1, available + 1),
                ] {
                    let query = format!("{seed}{}", "x".repeat(query_width - seed.width()));
                    let heading = query_divider(&query, 0, 2, theme, available);
                    let plain = without_sgr(&heading);
                    assert_eq!(
                        plain.width(),
                        expected_width,
                        "{terminal_columns} columns, query {query:?}: {plain:?}"
                    );
                    assert!(plain.contains(&query), "query must remain complete");
                    assert!(!plain.contains(['\n', '\r']), "one logical line");
                    assert!(
                        heading.contains(&format!("\x1b[1m{query}\x1b[0m")),
                        "bold query must retain the terminal foreground: {heading:?}"
                    );
                }
            }
        }
    }
}
