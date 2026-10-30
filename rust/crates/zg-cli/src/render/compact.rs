//! The original compact per-hit layout used within each query group.

use std::io::{self, Write};

use zg_engine::api::context::result::{
    ContentRange, ContextItem, ContextItemStatus, EntityMetadata,
};

use crate::{
    OutputOptions, PreviewMode, highlight::QueryHighlighter, output_path, theme::StatusTheme,
};

pub(super) fn write_items(
    writer: &mut impl Write,
    items: &[ContextItem],
    query: &str,
    options: OutputOptions,
    theme: StatusTheme,
) -> io::Result<()> {
    if items.is_empty() {
        return writeln!(writer, "No matches.");
    }
    let highlighter = QueryHighlighter::new(query, theme.color);
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            writeln!(writer)?;
        }
        let matched_by = serde_json::to_value(item.matched_by).map_err(io::Error::other)?;
        let header = format!(
            "#{} matchedBy={} {}:{}",
            item.rank,
            matched_by.as_str().unwrap_or_default(),
            output_path(&item.relative_path).display(),
            range_label(&item.range)
        );
        writeln!(writer, "{header}")?;
        write_item_preview(writer, item, options, &highlighter)?;
        if options.trace {
            if let Some(score) = item.score {
                writeln!(writer, "score: {score:.4}")?;
            }
            if let Some(trace) = &item.trace {
                writeln!(
                    writer,
                    "trace: {}",
                    serde_json::to_string(trace).map_err(io::Error::other)?
                )?;
            }
        }
    }
    Ok(())
}

pub(super) fn write_item_preview(
    writer: &mut impl Write,
    item: &ContextItem,
    options: OutputOptions,
    highlighter: &QueryHighlighter,
) -> io::Result<()> {
    if item.status == ContextItemStatus::PossiblyStale {
        writeln!(writer, "status: possibly_stale")?;
    }
    match &item.metadata {
        Some(EntityMetadata::Code(code)) => {
            if let Some(name) = &code.symbol_name {
                write!(writer, "symbol: ")?;
                if let Some(kind) = code.symbol_type {
                    let kind = serde_json::to_value(kind).map_err(io::Error::other)?;
                    write!(writer, "{} ", kind.as_str().unwrap_or_default())?;
                }
                write!(writer, "{name}")?;
                if let Some(scope) = &code.scope {
                    write!(writer, " scope: {scope}")?;
                }
                writeln!(writer)?;
            }
        }
        Some(EntityMetadata::Markdown(markdown)) => {
            if let Some(heading) = &markdown.heading {
                writeln!(writer, "heading: {heading}")?;
            }
            if let Some(level) = markdown.level {
                writeln!(writer, "heading_level: {level}")?;
            }
            if let Some(scope) = &markdown.scope {
                writeln!(writer, "scope: {scope}")?;
            }
        }
        None => {}
    }
    let short = options.preview == PreviewMode::Short;
    let max_lines = if short { 10 } else { usize::MAX };
    let first = item.content_range.start_line();
    let lines: Vec<_> = item.content.lines().collect();
    let anchor = item
        .excerpt_range
        .as_ref()
        .unwrap_or(&item.range)
        .start_line()
        .zip(first)
        .map_or(0, |(matched, first)| matched.saturating_sub(first))
        .min(lines.len().saturating_sub(1));
    let from = if short { anchor.saturating_sub(3) } else { 0 };
    for (offset, line) in lines.iter().enumerate().skip(from).take(max_lines) {
        let line = if short {
            line.chars().take(160).collect()
        } else {
            (*line).to_owned()
        };
        let line = highlighter.highlight(&line);
        if let Some(first) = first {
            writeln!(writer, "  {}: {line}", first + offset)?;
        } else {
            writeln!(writer, "  {line}")?;
        }
    }
    if short && lines.len() > from + max_lines {
        writeln!(writer, "  …")?;
    }
    Ok(())
}

fn range_label(range: &ContentRange) -> String {
    if let (Some(first), Some(last)) = (range.start_line(), range.last_line()) {
        return format!("{first}-{last}");
    }
    if let ContentRange::Byte {
        start_offset,
        end_offset,
    } = range
    {
        return format!("bytes:{start_offset}-{end_offset}");
    }
    "file".to_owned()
}
