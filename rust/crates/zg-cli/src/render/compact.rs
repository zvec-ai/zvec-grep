//! The original compact per-hit layout used within each query group.

use std::io::{self, Write};

use zg_engine::api::context::result::{
    ContentRange, ContextItem, ContextItemStatus, EntityMetadata,
};

use crate::{
    OutputOptions, PreviewMode, highlight::QueryHighlighter, output_path,
    preview::bounded_source_preview, theme::StatusTheme,
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
        let header = format!(
            "#{} {}:{}",
            item.rank,
            output_path(&item.relative_path).display(),
            range_label(&item.range)
        );
        if theme.color {
            writeln!(writer, "\x1b[1;36m{header}\x1b[0m")?;
        } else {
            writeln!(writer, "{header}")?;
        }
        write_item_preview(writer, item, options, &highlighter, theme)?;
        if options.trace {
            if let Some(score) = item.score {
                writeln!(writer, "{} {score:.4}", theme.metadata_label("score:"))?;
            }
            if let Some(trace) = &item.trace {
                writeln!(
                    writer,
                    "{} {}",
                    theme.metadata_label("trace:"),
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
    theme: StatusTheme,
) -> io::Result<()> {
    if item.status == ContextItemStatus::PossiblyStale {
        writeln!(writer, "{} possibly_stale", theme.metadata_label("status:"))?;
    }
    match &item.metadata {
        Some(EntityMetadata::Code(code)) => {
            if let Some(name) = &code.symbol_name {
                write!(writer, "{} ", theme.metadata_label("symbol:"))?;
                if let Some(kind) = code.symbol_type {
                    let kind = serde_json::to_value(kind).map_err(io::Error::other)?;
                    write!(writer, "[{}] ", kind.as_str().unwrap_or_default())?;
                }
                write!(writer, "{name}")?;
                if let Some(scope) = &code.scope {
                    write!(writer, " {} {scope}", theme.metadata_label("scope:"))?;
                }
                writeln!(writer)?;
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
    if options.preview == PreviewMode::Short || item.source_preview.is_some() {
        let source = bounded_source_preview(item, options.preview);
        for line in &source.lines {
            let text = highlighter.highlight(&line.text);
            if let Some(number) = line.line_number {
                writeln!(writer, "  {number}: {text}")?;
            } else {
                writeln!(writer, "  {text}")?;
            }
        }
        if source.trailing_omission {
            writeln!(writer, "  …")?;
        }
        return Ok(());
    }
    let first = item.content_range.start_line();
    for (offset, line) in item.content.lines().enumerate() {
        let line = highlighter.highlight(line);
        if let Some(first) = first {
            writeln!(writer, "  {}: {line}", first + offset)?;
        } else {
            writeln!(writer, "  {line}")?;
        }
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
