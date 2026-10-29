use crate::ColorMode;
use crossterm::terminal;
use std::fmt::Write as _;
use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use zg_engine::api::index::progress::{
    IndexEmbeddingStage, IndexProgress, IndexProgressPhase, IndexProgressReporter,
};

/// Owns terminal progress state and finishes the live line on success or failure.
pub struct IndexProgressDisplay {
    state: Arc<Mutex<ProgressOutput>>,
}

struct ProgressOutput {
    writer: Box<dyn Write + Send>,
    terminal: bool,
    color: bool,
    last: Option<(String, Instant, String)>,
    finished: bool,
    last_indexing: Option<IndexProgress>,
}

impl IndexProgressDisplay {
    #[must_use]
    pub fn new(writer: impl Write + Send + 'static, terminal: bool, color: ColorMode) -> Self {
        Self {
            state: Arc::new(Mutex::new(ProgressOutput {
                writer: Box::new(writer),
                terminal,
                color: color == ColorMode::Always
                    || (color == ColorMode::Auto
                        && terminal
                        && std::env::var_os("NO_COLOR").is_none()),
                last: None,
                finished: false,
                last_indexing: None,
            })),
        }
    }

    #[must_use]
    pub fn reporter(&self) -> IndexProgressReporter {
        let state = Arc::clone(&self.state);
        IndexProgressReporter::new(move |progress| {
            if let Ok(mut output) = state.lock() {
                // Progress output is best-effort and must not abort index work.
                let _ = output.report(&progress);
            }
        })
    }

    pub fn finish(&self) {
        if let Ok(mut output) = self.state.lock() {
            let _ = output.finish();
        }
    }
}

impl Drop for IndexProgressDisplay {
    fn drop(&mut self) {
        self.finish();
    }
}

impl ProgressOutput {
    fn report(&mut self, progress: &IndexProgress) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        let (stage, plain) = format_progress(progress, self.terminal);
        let line = if self.terminal {
            format_terminal_progress(
                progress,
                self.last_indexing.as_ref(),
                self.color,
                terminal::size().map_or_else(
                    |_| {
                        std::env::var("COLUMNS")
                            .ok()
                            .and_then(|value| value.parse::<usize>().ok())
                            .unwrap_or(100)
                    },
                    |(columns, _)| usize::from(columns),
                ),
            )
        } else {
            plain
        };
        if progress.phase == IndexProgressPhase::Indexing
            && progress.files_total.is_some()
            && progress
                .embedding
                .as_ref()
                .is_none_or(|embedding| embedding.stage.is_none())
        {
            self.last_indexing = Some(progress.clone());
        }
        let now = Instant::now();
        let interval = if self.terminal {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(15)
        };
        if let Some((previous_stage, time, previous_line)) = &self.last
            && previous_stage == &stage
            && (previous_line == &line || now.duration_since(*time) < interval)
        {
            return Ok(());
        }
        if self.terminal {
            let frame = self.last.as_ref().and_then(|(_, _, previous)| {
                terminal_change_start(previous, &line)
                    .map(|(byte, column)| format!("\x1b[{}G{}\x1b[K", column + 1, &line[byte..]))
            });
            let frame = frame.unwrap_or_else(|| format!("\r{line}\x1b[K"));
            self.writer.write_all(frame.as_bytes())?;
        } else {
            writeln!(self.writer, "{line}")?;
        }
        self.writer.flush()?;
        self.last = Some((stage, now, line));
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        if !self.finished && self.terminal && self.last.is_some() {
            write!(self.writer, "\r\n")?;
            self.writer.flush()?;
        }
        self.finished = true;
        Ok(())
    }
}

fn green(value: &str, color: bool) -> String {
    if color {
        format!("\x1b[38;2;74;222;128m{value}\x1b[0m")
    } else {
        value.into()
    }
}

pub(crate) fn gradient_bar(filled: usize, width: usize, color: bool, unicode: bool) -> String {
    let filled = filled.min(width);
    let (solid, empty) = if unicode { ("█", "░") } else { ("#", "-") };
    if !color {
        return solid.repeat(filled) + &empty.repeat(width - filled);
    }
    let mut bar = String::new();
    for index in 0..filled {
        // Keep each cell's color stable as the bar fills.
        let interpolate = |start: usize, end: usize| {
            if width <= 1 {
                end
            } else {
                start + ((end - start) * index + (width - 1) / 2) / (width - 1)
            }
        };
        let _ = write!(
            bar,
            "\x1b[38;2;{};{};{}m{solid}",
            interpolate(22, 134),
            interpolate(163, 239),
            interpolate(74, 172)
        );
    }
    let _ = write!(bar, "\x1b[0m\x1b[2m{}\x1b[0m", empty.repeat(width - filled));
    bar
}

fn format_terminal_progress(
    progress: &IndexProgress,
    previous: Option<&IndexProgress>,
    color: bool,
    columns: usize,
) -> String {
    let unicode = !cfg!(windows) && std::env::var("TERM").as_deref() != Ok("linux");
    let rail = if unicode { "│" } else { "|" };
    let rail = if color {
        format!("\x1b[2m{rail}\x1b[0m")
    } else {
        rail.into()
    };
    let spinner = if unicode {
        ["·", "✢", "✳", "✶", "✻", "✽"]
    } else {
        [".", "*", "+", "x", "o", "O"]
    };
    if progress.phase == IndexProgressPhase::Scanning {
        let count = progress
            .files_total
            .filter(|count| *count > 0)
            .map_or_else(String::new, |count| format!("  {count} files"));
        return fit_terminal_line(
            &format!(
                "{rail}  {} Scanning workspace{count}",
                green(spinner[0], color)
            ),
            columns,
        );
    }
    if let Some(embedding) = &progress.embedding
        && embedding.stage.is_some()
    {
        let index = usize::try_from(embedding.downloaded_bytes.unwrap_or(0) % 6).unwrap_or(0);
        return fit_terminal_line(
            &format!(
                "{rail}  {} {}",
                green(spinner[index], color),
                format_progress(progress, true).1
            ),
            columns,
        );
    }
    let done = progress.phase == IndexProgressPhase::Done;
    let effective = if done || progress.files_total.is_none() {
        previous.unwrap_or(progress)
    } else {
        progress
    };
    let total = effective.files_total.unwrap_or(0);
    let indexed = if done {
        total
    } else {
        effective.files_indexed.unwrap_or(0).min(total)
    };
    let glyph = if done {
        if unicode { "◆" } else { "*" }
    } else {
        spinner[0]
    };
    let prefix = format!("{rail}  {} Indexing files  ", green(glyph, color));
    if total == 0 {
        let label = if done {
            progress.detail.as_deref().unwrap_or("Indexing complete")
        } else {
            "Indexing files"
        };
        return fit_terminal_line(
            &format!("{rail}  {} {}", green(glyph, color), clean(label, true)),
            columns,
        );
    }
    let percent = (indexed as u128 * 100 + total as u128 / 2) / total as u128;
    let core = format!("  {percent:>3}%  {indexed}/{total}");
    let metadata = indexing_metadata(effective);
    let mut suffix = format!("{core}{metadata}");
    let available = |suffix: &str| columns.saturating_sub(22 + suffix.chars().count());
    if available(&core) == 0 {
        return fit_terminal_line(
            &format!(
                "{rail}  {} {indexed}/{total} ({percent}%)",
                green(glyph, color)
            ),
            columns,
        );
    }
    if available(&suffix) < 8 {
        suffix = core;
    }
    let width = available(&suffix).min(25);
    let filled = usize::try_from((width as u128 * percent + 50) / 100).unwrap_or(width);
    fit_terminal_line(
        &format!(
            "{prefix}{}{suffix}",
            gradient_bar(filled, width, color, unicode)
        ),
        columns,
    )
}

fn indexing_metadata(progress: &IndexProgress) -> String {
    let mut metadata = Vec::new();
    if let Some(failed) = progress.files_failed.filter(|count| *count > 0) {
        metadata.push(format!("{failed} failed"));
    }
    if let Some(embedding) = &progress.embedding {
        if let Some(retries) = embedding.retryable_failures.filter(|count| *count > 0) {
            metadata.push(format!("{retries} retries"));
        }
        if let Some(workers) = embedding.concurrency.filter(|count| *count > 0) {
            metadata.push(format!("{workers} workers"));
        }
    }
    if metadata.is_empty() {
        String::new()
    } else {
        format!("  {}", metadata.join(" · "))
    }
}

fn fit_terminal_line(line: &str, columns: usize) -> String {
    let mut fitted = String::with_capacity(line.len());
    let mut characters = line.chars().peekable();
    let mut occupied = 0;
    let limit = columns.saturating_sub(1);
    let mut clipped = false;
    while let Some(character) = characters.next() {
        if character == '\x1b' && characters.peek() == Some(&'[') {
            fitted.push(character);
            fitted.push(characters.next().expect("CSI introducer"));
            for code in characters.by_ref() {
                fitted.push(code);
                if ('@'..='~').contains(&code) {
                    break;
                }
            }
            continue;
        }
        let width = terminal_character_width(character).unwrap_or(2);
        if occupied + width > limit {
            clipped = true;
            break;
        }
        fitted.push(character);
        occupied += width;
    }
    if clipped && fitted.contains("\x1b[") {
        fitted.push_str("\x1b[0m");
    }
    fitted
}

fn terminal_change_start(previous: &str, next: &str) -> Option<(usize, usize)> {
    let mut common = 0;
    for (left, right) in previous.chars().zip(next.chars()) {
        if left != right {
            break;
        }
        common += left.len_utf8();
    }
    // A completed reset needs no replay because each previous frame ends in the default style.
    let start = next[..common]
        .rfind("\x1b[")
        .filter(|&escape| !next[escape..common].starts_with("\x1b[0m"))
        .unwrap_or(common);
    let column = terminal_visible_width(&next[..start])?;
    (column > 0).then_some((start, column))
}

fn terminal_visible_width(value: &str) -> Option<usize> {
    let mut characters = value.chars().peekable();
    let mut width = 0;
    while let Some(character) = characters.next() {
        if character == '\x1b' && characters.peek() == Some(&'[') {
            characters.next();
            let mut complete = false;
            for code in characters.by_ref() {
                if ('@'..='~').contains(&code) {
                    complete = true;
                    break;
                }
            }
            if !complete {
                return None;
            }
        } else {
            width += terminal_character_width(character)?;
        }
    }
    Some(width)
}

fn terminal_character_width(character: char) -> Option<usize> {
    if character.is_control() {
        Some(0)
    } else if character.is_ascii()
        || matches!(
            character,
            '│' | '·' | '✢' | '✳' | '✶' | '✻' | '✽' | '◆' | '█' | '░'
        )
    {
        Some(1)
    } else {
        None
    }
}

fn format_progress(progress: &IndexProgress, terminal: bool) -> (String, String) {
    if let Some(embedding) = &progress.embedding
        && let Some(stage) = embedding.stage
    {
        let model = clean(
            embedding.model.as_deref().unwrap_or("embedding model"),
            terminal,
        );
        let label = match stage {
            IndexEmbeddingStage::Preparing => "Preparing model",
            IndexEmbeddingStage::Downloading => "Downloading model",
            IndexEmbeddingStage::Ready => "Model ready",
            IndexEmbeddingStage::Warning => "Model warning",
        };
        let mut line = format!("{label}: {model}");
        if let Some(downloaded) = embedding.downloaded_bytes {
            if let Some(total) = embedding.total_bytes.filter(|total| *total > 0) {
                let percent = downloaded
                    .saturating_mul(100)
                    .checked_div(total)
                    .unwrap_or(0)
                    .min(100);
                let _ = write!(line, " {percent}% ({}/{})", bytes(downloaded), bytes(total));
            } else {
                let _ = write!(line, " {}", bytes(downloaded));
            }
        }
        if let Some(message) = &embedding.message {
            let _ = write!(line, " — {}", clean(message, terminal));
        }
        return (label.into(), line);
    }
    let label = match progress.phase {
        IndexProgressPhase::Scanning => "Scanning",
        IndexProgressPhase::Indexing => "Indexing",
        IndexProgressPhase::Done => "Index complete",
    };
    let mut line = label.to_owned();
    if let Some(total) = progress.files_total {
        if let Some(indexed) = progress.files_indexed {
            let _ = write!(line, ": {indexed}/{total} files");
        } else {
            let _ = write!(line, ": {total} files");
        }
    }
    if let Some(failed) = progress.files_failed.filter(|failed| *failed > 0) {
        let _ = write!(line, ", {failed} failed");
    }
    if let Some(detail) = &progress.detail {
        let _ = write!(line, " — {}", clean(detail, terminal));
    }
    (label.into(), line)
}

fn bytes(value: u64) -> String {
    if value >= 1024 * 1024 {
        format!("{} MiB", value / (1024 * 1024))
    } else if value >= 1024 {
        format!("{} KiB", value / 1024)
    } else {
        format!("{value} B")
    }
}

fn clean(value: &str, terminal: bool) -> String {
    // Keep live terminal output compact, but retain complete diagnostics in logs.
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(if terminal { 100 } else { usize::MAX })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zg_engine::api::index::progress::IndexEmbeddingProgress;
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("capture").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn gradient_matches_reference_colors_and_plain_glyphs() {
        let bar = gradient_bar(3, 5, true, true);
        assert_eq!(
            bar,
            "\x1b[38;2;22;163;74m█\x1b[38;2;50;182;99m█\x1b[38;2;78;201;123m█\x1b[0m\x1b[2m░░\x1b[0m"
        );
        assert_eq!(gradient_bar(3, 5, false, true), "███░░");
        assert_eq!(gradient_bar(3, 5, false, false), "###--");
        assert!(gradient_bar(1, 2, true, true).starts_with("\x1b[38;2;22;163;74m"));
        let existing_fill = bar.split("\x1b[0m").next().expect("colored fill");
        assert!(
            gradient_bar(4, 5, true, true).starts_with(existing_fill),
            "existing bar cells should keep their color as the bar grows"
        );
    }

    #[test]
    fn terminal_progress_fits_a_narrow_window() {
        let progress = IndexProgress {
            phase: IndexProgressPhase::Indexing,
            files_total: Some(8),
            files_indexed: Some(3),
            files_failed: None,
            detail: None,
            embedding: None,
        };
        let line = format_terminal_progress(&progress, None, false, 40);
        assert!(
            line.chars().count() < 40,
            "progress wraps in a 40-column terminal: {line}"
        );
        let compact = format_terminal_progress(&progress, None, false, 24);
        assert!(
            compact.contains("3/8"),
            "narrow progress loses the indexed count: {compact}"
        );
    }

    #[test]
    fn output_is_throttled_and_live_line_finishes_on_drop() {
        for terminal in [false, true] {
            let buffer = Arc::new(Mutex::new(Vec::new()));
            let display =
                IndexProgressDisplay::new(Capture(buffer.clone()), terminal, ColorMode::Never);
            let reporter = display.reporter();
            let progress = IndexProgress {
                phase: IndexProgressPhase::Scanning,
                files_total: Some(1),
                files_indexed: None,
                files_failed: None,
                detail: None,
                embedding: None,
            };
            reporter.report(progress.clone());
            reporter.report(progress.clone());
            reporter.report(IndexProgress {
                phase: IndexProgressPhase::Done,
                ..progress
            });
            drop(display);
            let output = String::from_utf8(buffer.lock().expect("capture").clone()).expect("UTF-8");
            assert_eq!(output.matches("Scanning").count(), 1);
            assert!(output.contains(if terminal { "100%" } else { "Index complete" }));
            assert!(output.ends_with('\n'));
            assert_eq!(output.contains("\x1b[K"), terminal);
            assert!(!output.contains("\r\x1b[2K"));
            assert!(!output.contains("\x1b[36m"));
        }
    }

    #[test]
    fn indexing_updates_do_not_repaint_the_stable_progress_prefix() {
        for color in [false, true] {
            for (indexed, total, changed_suffix) in [(670, 1045, "1/1045"), (892, 20936, "3/20936")]
            {
                let buffer = Arc::new(Mutex::new(Vec::new()));
                let mut output = ProgressOutput {
                    writer: Box::new(Capture(buffer.clone())),
                    terminal: true,
                    color,
                    last: None,
                    finished: false,
                    last_indexing: None,
                };
                let progress = IndexProgress {
                    phase: IndexProgressPhase::Indexing,
                    files_total: Some(total),
                    files_indexed: Some(indexed),
                    files_failed: None,
                    detail: None,
                    embedding: None,
                };
                output.report(&progress).expect("first progress frame");
                buffer.lock().expect("capture").clear();
                output.last.as_mut().expect("last frame").1 =
                    Instant::now() - Duration::from_millis(200);
                output
                    .report(&IndexProgress {
                        files_indexed: Some(indexed + 1),
                        ..progress
                    })
                    .expect("next progress frame");
                let frame = String::from_utf8(buffer.lock().expect("capture").clone())
                    .expect("UTF-8 progress frame");
                assert!(frame.starts_with("\x1b["), "{frame:?}");
                assert!(
                    frame.ends_with(&format!("{changed_suffix}\x1b[K")),
                    "{frame:?}"
                );
                assert!(!frame.contains("Indexing files"), "{frame:?}");
                assert!(!frame.contains('█'), "{frame:?}");
                assert!(!frame.contains('░'), "{frame:?}");
                assert!(!frame.contains('%'), "{frame:?}");
                assert!(!frame.contains("\x1b[0m"), "{frame:?}");
            }
        }
    }

    #[test]
    fn embedding_batch_progress_keeps_the_indexing_bar_visible() {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let mut output = ProgressOutput {
            writer: Box::new(Capture(buffer.clone())),
            terminal: true,
            color: true,
            last: None,
            finished: false,
            last_indexing: None,
        };
        let indexing = IndexProgress {
            phase: IndexProgressPhase::Indexing,
            files_total: Some(20_936),
            files_indexed: Some(892),
            files_failed: Some(0),
            detail: None,
            embedding: None,
        };
        output.report(&indexing).expect("indexing progress");
        buffer.lock().expect("capture").clear();
        output.last.as_mut().expect("last frame").1 = Instant::now() - Duration::from_millis(200);
        let batch = IndexProgress {
            files_total: None,
            files_indexed: None,
            files_failed: None,
            detail: Some("embedding files".into()),
            embedding: Some(IndexEmbeddingProgress {
                concurrency: Some(8),
                ..IndexEmbeddingProgress::default()
            }),
            ..indexing.clone()
        };
        output.report(&batch).expect("embedding batch progress");
        let line = &output.last.as_ref().expect("last frame").2;
        assert!(line.contains('█'), "{line:?}");
        assert!(line.contains("892/20936"), "{line:?}");
        assert!(buffer.lock().expect("capture").is_empty());
        assert_eq!(
            output
                .last_indexing
                .as_ref()
                .expect("indexed counts")
                .files_indexed,
            Some(892)
        );
        output.last.as_mut().expect("last frame").1 = Instant::now() - Duration::from_millis(200);
        output
            .report(&IndexProgress {
                embedding: Some(IndexEmbeddingProgress {
                    concurrency: Some(7),
                    ..IndexEmbeddingProgress::default()
                }),
                ..batch
            })
            .expect("next embedding batch progress");
        assert!(buffer.lock().expect("capture").is_empty());
        output
            .report(&IndexProgress {
                files_indexed: Some(893),
                ..indexing
            })
            .expect("next indexed count");
        let frame = String::from_utf8(buffer.lock().expect("capture").clone())
            .expect("UTF-8 progress frame");
        assert!(frame.contains("3/20936"), "{frame:?}");
        assert!(!frame.contains("Indexing files"), "{frame:?}");
    }

    #[test]
    fn warning_output_is_complete_unless_interactive() {
        let warning = "Transformers cuda embedding initialization failed (Unable to configure CUDA ONNX embedding model), falling back to CPU.";
        for terminal in [false, true] {
            let buffer = Arc::new(Mutex::new(Vec::new()));
            let display =
                IndexProgressDisplay::new(Capture(buffer.clone()), terminal, ColorMode::Never);
            display.reporter().report(IndexProgress {
                phase: IndexProgressPhase::Indexing,
                files_total: None,
                files_indexed: None,
                files_failed: None,
                detail: None,
                embedding: Some(IndexEmbeddingProgress {
                    stage: Some(IndexEmbeddingStage::Warning),
                    model: Some("local/all-minilm-l6-v2".into()),
                    message: Some(warning.into()),
                    ..IndexEmbeddingProgress::default()
                }),
            });
            drop(display);
            let output = String::from_utf8(buffer.lock().expect("capture").clone()).expect("UTF-8");
            if terminal {
                assert!(output.contains("Transformers cuda"));
                assert!(!output.contains(warning));
            } else {
                assert!(output.contains(warning));
                assert!(output.contains("falling back to CPU."));
            }
            assert!(output.ends_with('\n'));
        }
    }

    #[test]
    fn plain_progress_preserves_long_fields_and_sanitizes_controls() {
        let long_text = format!("{}\n\t\r\u{1b}tail", "模".repeat(120));
        let sanitized = format!("{}    tail", "模".repeat(120));
        let mut progress = IndexProgress {
            phase: IndexProgressPhase::Done,
            files_total: None,
            files_indexed: None,
            files_failed: None,
            detail: Some(long_text.clone()),
            embedding: None,
        };
        assert_eq!(
            format_progress(&progress, false).1,
            format!("Index complete — {sanitized}")
        );
        progress.embedding = Some(IndexEmbeddingProgress {
            stage: Some(IndexEmbeddingStage::Warning),
            model: Some(long_text.clone()),
            message: Some(long_text),
            ..IndexEmbeddingProgress::default()
        });
        assert_eq!(
            format_progress(&progress, false).1,
            format!("Model warning: {sanitized} — {sanitized}")
        );
        let compact = "模".repeat(100);
        assert_eq!(
            format_progress(&progress, true).1,
            format!("Model warning: {compact} — {compact}")
        );
    }

    #[test]
    fn formats_download_bytes_and_index_counts() {
        let mut progress = IndexProgress {
            phase: IndexProgressPhase::Indexing,
            files_total: Some(8),
            files_indexed: Some(3),
            files_failed: Some(1),
            detail: None,
            embedding: None,
        };
        assert_eq!(
            format_progress(&progress, false).1,
            "Indexing: 3/8 files, 1 failed"
        );
        progress.embedding = Some(IndexEmbeddingProgress {
            stage: Some(IndexEmbeddingStage::Downloading),
            model: Some("local/model".into()),
            downloaded_bytes: Some(1024),
            total_bytes: Some(2048),
            ..IndexEmbeddingProgress::default()
        });
        assert_eq!(
            format_progress(&progress, false).1,
            "Downloading model: local/model 50% (1 KiB/2 KiB)"
        );
        progress.embedding.as_mut().expect("embedding").total_bytes = None;
        assert!(format_progress(&progress, false).1.ends_with("1 KiB"));
    }
}
