use crate::ColorMode;
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
        let (stage, plain) = format_progress(progress);
        let line = if self.terminal {
            format_terminal_progress(progress, self.last_indexing.as_ref(), self.color)
        } else {
            plain
        };
        if progress.phase == IndexProgressPhase::Indexing
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
            write!(self.writer, "\r\x1b[2K")?;
        }
        write!(self.writer, "{line}")?;
        if !self.terminal {
            writeln!(self.writer)?;
        }
        self.writer.flush()?;
        self.last = Some((stage, now, line));
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        if !self.finished && self.terminal && self.last.is_some() {
            writeln!(self.writer)?;
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

fn gradient_bar(filled: usize, width: usize, color: bool, unicode: bool) -> String {
    let filled = filled.min(width);
    let (solid, empty) = if unicode { ("█", "░") } else { ("#", "-") };
    if !color {
        return solid.repeat(filled) + &empty.repeat(width - filled);
    }
    let mut bar = String::new();
    for index in 0..filled {
        // Match the reference's rounded interpolation across the filled portion.
        let interpolate = |start: usize, end: usize| {
            if filled <= 1 {
                end
            } else {
                start + ((end - start) * index + (filled - 1) / 2) / (filled - 1)
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
        return format!(
            "{rail}  {} Scanning workspace{count}",
            green(spinner[0], color)
        );
    }
    if let Some(embedding) = &progress.embedding
        && embedding.stage.is_some()
    {
        let index = usize::try_from(embedding.downloaded_bytes.unwrap_or(0) % 6).unwrap_or(0);
        return format!(
            "{rail}  {} {}",
            green(spinner[index], color),
            format_progress(progress).1
        );
    }
    let done = progress.phase == IndexProgressPhase::Done;
    let effective = if done {
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
        spinner[indexed % spinner.len()]
    };
    let prefix = format!("{rail}  {} Indexing files  ", green(glyph, color));
    if total == 0 {
        let label = if done {
            progress.detail.as_deref().unwrap_or("Indexing complete")
        } else {
            "Indexing files"
        };
        return format!("{rail}  {} {}", green(glyph, color), clean(label));
    }
    let percent = (indexed as u128 * 100 + total as u128 / 2) / total as u128;
    let core = format!("  {percent:>3}%  {indexed}/{total}");
    let mut metadata = Vec::new();
    if let Some(failed) = effective.files_failed.filter(|count| *count > 0) {
        metadata.push(format!("{failed} failed"));
    }
    if let Some(embedding) = &effective.embedding {
        if let Some(retries) = embedding.retryable_failures.filter(|count| *count > 0) {
            metadata.push(format!("{retries} retries"));
        }
        if let Some(workers) = embedding.concurrency.filter(|count| *count > 0) {
            metadata.push(format!("{workers} workers"));
        }
    }
    let metadata = if metadata.is_empty() {
        String::new()
    } else {
        format!("  {}", metadata.join(" · "))
    };
    let columns = std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(100);
    let mut suffix = format!("{core}{metadata}");
    let available = |suffix: &str| columns.saturating_sub(22 + suffix.chars().count());
    if available(&suffix) < 8 {
        suffix = core;
    }
    let width = available(&suffix).clamp(8, 25);
    let filled = usize::try_from((width as u128 * percent + 50) / 100).unwrap_or(width);
    format!(
        "{prefix}{}{suffix}",
        gradient_bar(filled, width, color, unicode)
    )
}

fn format_progress(progress: &IndexProgress) -> (String, String) {
    if let Some(embedding) = &progress.embedding
        && let Some(stage) = embedding.stage
    {
        let model = clean(embedding.model.as_deref().unwrap_or("embedding model"));
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
            let _ = write!(line, " — {}", clean(message));
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
        let _ = write!(line, " — {}", clean(detail));
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

fn clean(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(100)
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
            "\x1b[38;2;22;163;74m█\x1b[38;2;78;201;123m█\x1b[38;2;134;239;172m█\x1b[0m\x1b[2m░░\x1b[0m"
        );
        assert_eq!(gradient_bar(3, 5, false, true), "███░░");
        assert_eq!(gradient_bar(3, 5, false, false), "###--");
        assert!(gradient_bar(1, 2, true, true).starts_with("\x1b[38;2;134;239;172m"));
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
            assert_eq!(output.contains("\r\x1b[2K"), terminal);
            assert!(!output.contains("\x1b[36m"));
        }
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
            format_progress(&progress).1,
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
            format_progress(&progress).1,
            "Downloading model: local/model 50% (1 KiB/2 KiB)"
        );
        progress.embedding.as_mut().expect("embedding").total_bytes = None;
        assert!(format_progress(&progress).1.ends_with("1 KiB"));
    }
}
