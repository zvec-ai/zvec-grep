//! The original compact per-hit layout used within each query group.

use std::io::{self, Write};

use zg_engine::api::context::result::{
    ContentRange, ContextItem, ContextItemStatus, EntityMetadata,
};

use crate::{
    OutputOptions, PreviewMode, highlight::QueryHighlighter, output_path,
    preview::short_source_line, theme::StatusTheme,
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
        let suffix = if matches!(item.range, ContentRange::Pdf { .. }) {
            String::new()
        } else {
            format!(":{}", range_label(&item.range))
        };
        let header = format!(
            "#{} {}{}",
            item.rank,
            output_path(&item.relative_path).display(),
            suffix
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
    if let ContentRange::Pdf { pages, .. } = &item.range {
        writeln!(
            writer,
            "{} {}",
            theme.metadata_label("pages:"),
            pages
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        )?;
        writeln!(writer, "{} text/native", theme.metadata_label("kind:"))?;
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
        Some(EntityMetadata::Markdown(markdown))
            if !matches!(item.range, ContentRange::Pdf { .. }) =>
        {
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
        Some(EntityMetadata::Markdown(_)) | None => {}
    }
    let short = options.preview == PreviewMode::Short;
    let max_lines = if short { 10 } else { usize::MAX };
    let first = item.content_range.start_line();
    let lines: Vec<_> = item.content.lines().collect();
    let anchor = crate::preview::pdf_excerpt_position(item)
        .map_or_else(
            || {
                item.excerpt_range
                    .as_ref()
                    .unwrap_or(&item.range)
                    .start_line()
                    .zip(first)
                    .map_or(0, |(matched, first)| matched.saturating_sub(first))
            },
            |(line, _)| line,
        )
        .min(lines.len().saturating_sub(1));
    let available = if short {
        crate::preview::pdf_excerpt_lines(item).unwrap_or(0..lines.len())
    } else {
        0..lines.len()
    };
    let from = if short {
        anchor.saturating_sub(3).max(available.start)
    } else {
        0
    };
    let end = available.end.min(lines.len());
    for (offset, line) in lines
        .iter()
        .enumerate()
        .take(end)
        .skip(from)
        .take(max_lines)
    {
        let line = if short {
            short_source_line(item, offset, line)
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
    if short && end > from + max_lines {
        writeln!(writer, "  …")?;
    }
    Ok(())
}

fn range_label(range: &ContentRange) -> String {
    if let ContentRange::Pdf { pages, .. } = range {
        return format!(
            "pages:{}",
            pages
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );
    }
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
