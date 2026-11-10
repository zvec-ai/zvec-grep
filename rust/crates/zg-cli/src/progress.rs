use crate::{ColorMode, terminal::terminal_character_width};
use std::fmt::Write as _;
use std::{
    io::{self, Write},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use zg_engine::api::index::progress::{
    DETAIL_FINALIZING_INDEX, DETAIL_INDEXING_COMPLETE, DETAIL_SAVING_INDEX, IndexEmbeddingStage,
    IndexProgress, IndexProgressPhase, IndexProgressReporter,
};

/// Retains major index stages and finishes the active line on success or failure.
pub struct IndexProgressDisplay {
    state: ProgressState,
    worker: Mutex<Option<JoinHandle<()>>>,
}

const FRAME_INTERVAL: Duration = Duration::from_millis(125);
const MAX_PENDING_STAGES: usize = 16;

#[derive(Clone)]
enum ProgressState {
    Terminal(Arc<(Mutex<PendingProgress>, Condvar)>),
    Plain(Arc<Mutex<PlainOutput>>),
}

#[derive(Default)]
struct PendingProgress {
    latest: Option<IndexProgress>,
    transitions: Vec<IndexProgress>,
    files_total: Option<usize>,
    files_indexed: Option<usize>,
    files_failed: Option<usize>,
    warnings: Vec<IndexProgress>,
    started: bool,
    finished: bool,
}

impl PendingProgress {
    fn update_latest(&mut self, mut progress: IndexProgress) {
        // Keep counts across both coalesced events and consumed frames. Scanning
        // resets them, even when that event is replaced before the next frame.
        // Stage, detail, and embedding always belong to the newest event.
        if progress.phase != IndexProgressPhase::Scanning {
            progress.files_total = progress.files_total.or(self.files_total);
            progress.files_indexed = progress.files_indexed.or(self.files_indexed);
            progress.files_failed = progress.files_failed.or(self.files_failed);
        }
        self.files_total = progress.files_total;
        self.files_indexed = progress.files_indexed;
        self.files_failed = progress.files_failed;
        // Coalesce counts within a phase, but keep the last snapshot of phases
        // that finish between frames. Small workspaces must not lose their
        // entire history just because they finish before the next 8 Hz tick.
        if self
            .latest
            .as_ref()
            .is_some_and(|previous| TerminalStage::of(previous) != TerminalStage::of(&progress))
            && let Some(previous) = self.latest.take()
        {
            // Keep the UI queue bounded even if its writer is stalled while
            // an external reporter repeatedly changes phases.
            if self.transitions.len() == MAX_PENDING_STAGES {
                self.transitions.remove(0);
            }
            self.transitions.push(previous);
        }
        self.latest = Some(progress);
    }
}

struct PlainOutput {
    writer: Box<dyn Write + Send>,
    last: Option<(Stage, Instant, String)>,
    finished: bool,
}

impl IndexProgressDisplay {
    #[must_use]
    pub fn new(writer: impl Write + Send + 'static, terminal: bool, color: ColorMode) -> Self {
        let term = std::env::var("TERM").unwrap_or_default();
        if !terminal || term == "dumb" {
            return Self {
                state: ProgressState::Plain(Arc::new(Mutex::new(PlainOutput {
                    writer: Box::new(writer),
                    last: None,
                    finished: false,
                }))),
                worker: Mutex::new(None),
            };
        }
        Self::terminal(writer, color, !cfg!(windows) && term != "linux")
    }

    fn terminal(writer: impl Write + Send + 'static, color: ColorMode, unicode: bool) -> Self {
        let pending = Arc::new((Mutex::new(PendingProgress::default()), Condvar::new()));
        let state = Arc::clone(&pending);
        let worker = thread::spawn(move || {
            let mut output = TerminalOutput {
                writer: Box::new(writer),
                color: color == ColorMode::Always
                    || (color == ColorMode::Auto && std::env::var_os("NO_COLOR").is_none()),
                unicode,
                latest: None,
                stage: None,
                stage_started: Instant::now(),
                frame: String::with_capacity(512),
                previous_line: String::new(),
                visible: false,
                columns: 80,
                width_checked: None,
                tick: 0,
            };
            render_loop(&state, &mut output);
        });
        Self {
            state: ProgressState::Terminal(pending),
            worker: Mutex::new(Some(worker)),
        }
    }

    #[must_use]
    pub fn reporter(&self) -> IndexProgressReporter {
        let state = self.state.clone();
        IndexProgressReporter::new(move |progress| {
            match &state {
                ProgressState::Terminal(state) => {
                    let (pending, wake) = &**state;
                    let mut pending = pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if pending.finished {
                        return;
                    }
                    // The producer merges scalar counts and moves owned events.
                    // Formatting, terminal size queries, and I/O happen on the
                    // renderer, outside this short lock.
                    if Stage::of(&progress) == Stage::Warning {
                        pending.warnings.push(progress);
                    } else {
                        pending.update_latest(progress);
                    }
                    if !pending.started {
                        pending.started = true;
                        wake.notify_one();
                    }
                }
                ProgressState::Plain(output) => {
                    if let Ok(mut output) = output.lock() {
                        // Progress output is best-effort and must not abort index work.
                        let _ = output.report(&progress);
                    }
                }
            }
        })
    }

    pub fn finish(&self) {
        match &self.state {
            ProgressState::Terminal(state) => {
                let (pending, wake) = &**state;
                pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .finished = true;
                wake.notify_one();
                // Joining ensures the live line is cleared before a summary or an
                // error is printed. The condition variable makes this immediate.
                if let Some(worker) = self
                    .worker
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    let _ = worker.join();
                }
            }
            ProgressState::Plain(output) => {
                if let Ok(mut output) = output.lock() {
                    output.finished = true;
                }
            }
        }
    }
}

impl Drop for IndexProgressDisplay {
    fn drop(&mut self) {
        self.finish();
    }
}

impl PlainOutput {
    fn report(&mut self, progress: &IndexProgress) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        let stage = Stage::of(progress);
        let now = Instant::now();
        if let Some((previous_stage, time, _)) = &self.last
            && *previous_stage == stage
            && stage != Stage::Warning
            && now.duration_since(*time) < Duration::from_secs(15)
        {
            return Ok(());
        }
        let (_, line) = format_progress(progress, false);
        if stage != Stage::Warning
            && self
                .last
                .as_ref()
                .is_some_and(|(previous_stage, _, previous_line)| {
                    *previous_stage == stage && previous_line == &line
                })
        {
            return Ok(());
        }
        self.writer.write_all(format!("{line}\n").as_bytes())?;
        self.writer.flush()?;
        self.last = Some((stage, now, line));
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Scanning,
    Indexing,
    FilesProcessed,
    Optimizing,
    Saving,
    Finishing,
    Preparing,
    Downloading,
    ModelReady,
    Warning,
}

impl Stage {
    fn of(progress: &IndexProgress) -> Self {
        if progress.phase == IndexProgressPhase::Done {
            return Self::Finishing;
        }
        if progress.phase == IndexProgressPhase::Finalizing {
            return if progress.detail.as_deref() == Some(DETAIL_SAVING_INDEX) {
                Self::Saving
            } else {
                Self::Optimizing
            };
        }
        if let Some(stage) = progress
            .embedding
            .as_ref()
            .and_then(|embedding| embedding.stage)
        {
            return match stage {
                IndexEmbeddingStage::Preparing => Self::Preparing,
                IndexEmbeddingStage::Downloading => Self::Downloading,
                IndexEmbeddingStage::Ready => Self::ModelReady,
                IndexEmbeddingStage::Warning => Self::Warning,
            };
        }
        match (progress.phase, progress.detail.as_deref()) {
            (_, Some(DETAIL_FINALIZING_INDEX)) => Self::Optimizing,
            (_, Some(DETAIL_SAVING_INDEX)) => Self::Saving,
            (IndexProgressPhase::Scanning, _) => Self::Scanning,
            (IndexProgressPhase::Indexing, _)
                if progress.files_total.is_some_and(|total| {
                    total > 0 && progress.files_indexed.unwrap_or(0) >= total
                }) =>
            {
                Self::FilesProcessed
            }
            _ => Self::Indexing,
        }
    }
}

/// User-facing milestones. Model substeps and the optimize/save/cleanup tail
/// stay on one stable heading instead of rapidly replacing each other's labels.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TerminalStage {
    Scanning,
    Model,
    Indexing,
    Finalizing,
}

impl TerminalStage {
    fn of(progress: &IndexProgress) -> Self {
        match Stage::of(progress) {
            Stage::Scanning => Self::Scanning,
            Stage::Preparing | Stage::Downloading | Stage::ModelReady | Stage::Warning => {
                Self::Model
            }
            Stage::Indexing | Stage::FilesProcessed => Self::Indexing,
            Stage::Optimizing | Stage::Saving | Stage::Finishing => Self::Finalizing,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Scanning => "Scanning workspace",
            Self::Model => "Preparing model",
            Self::Indexing => "Indexing files",
            Self::Finalizing => "Finalizing index",
        }
    }
}

struct TerminalOutput {
    writer: Box<dyn Write + Send>,
    color: bool,
    unicode: bool,
    latest: Option<IndexProgress>,
    stage: Option<TerminalStage>,
    stage_started: Instant,
    frame: String,
    previous_line: String,
    visible: bool,
    columns: usize,
    width_checked: Option<Instant>,
    tick: usize,
}

fn render_loop(state: &(Mutex<PendingProgress>, Condvar), output: &mut TerminalOutput) {
    let (pending, wake) = state;
    let mut pending = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while !pending.started && !pending.finished {
        pending = wake
            .wait(pending)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    loop {
        let latest = pending.latest.take();
        let transitions = std::mem::take(&mut pending.transitions);
        let warnings = std::mem::take(&mut pending.warnings);
        let finished = pending.finished;
        drop(pending);
        let started = Instant::now();
        // In particular, no producer lock is held across a blocked terminal write.
        if output
            .render(transitions, latest, warnings, started, finished)
            .is_err()
            || finished
        {
            state
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .finished = true;
            let _ = output.finish();
            return;
        }
        pending = state
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remaining = FRAME_INTERVAL.saturating_sub(started.elapsed());
        (pending, _) = wake
            .wait_timeout_while(pending, remaining, |pending| !pending.finished)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

impl TerminalOutput {
    fn render(
        &mut self,
        transitions: Vec<IndexProgress>,
        latest: Option<IndexProgress>,
        warnings: Vec<IndexProgress>,
        now: Instant,
        finished: bool,
    ) -> io::Result<()> {
        self.frame.clear();
        for progress in transitions.into_iter().chain(latest) {
            let stage = TerminalStage::of(&progress);
            if self.stage != Some(stage) {
                if let Some(previous) = &self.latest
                    // The pipeline announces indexing before preparing its
                    // model. Do not retain that zero-work placeholder.
                    && !(stage == TerminalStage::Model
                        && self.stage == Some(TerminalStage::Indexing)
                        && previous.files_indexed.unwrap_or(0) == 0)
                {
                    // Replace the active row with a durable, non-animated
                    // record. No cursor-up movement or full-screen clearing:
                    // history can wrap and resize like ordinary terminal text.
                    self.frame.push('\r');
                    self.frame
                        .push_str(&stage_record(previous, self.color, self.unicode));
                    self.frame.push_str("\x1b[K\r\n");
                    self.visible = false;
                    self.previous_line.clear();
                }
                self.stage_started = now;
                self.tick = 0;
                self.stage = Some(stage);
            }
            self.latest = Some(progress);
        }
        if !warnings.is_empty() && self.visible {
            self.frame.push_str("\r\x1b[K");
        }
        let warned = !warnings.is_empty();
        for warning in warnings {
            // Warnings are durable output, never a replaceable animation frame.
            // Keep their complete diagnostic text even on narrow terminals.
            self.frame.push_str(if self.color {
                "\x1b[33m!\x1b[0m "
            } else {
                "! "
            });
            self.frame.push_str(&format_progress(&warning, false).1);
            self.frame.push_str("\r\n");
        }
        if !finished && let Some(progress) = &self.latest {
            if self
                .width_checked
                .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1))
            {
                let columns = crossterm::terminal::size()
                    .ok()
                    .map(|(width, _)| usize::from(width))
                    .filter(|width| *width > 0)
                    .or_else(|| {
                        std::env::var("COLUMNS")
                            .ok()
                            .and_then(|value| value.parse().ok())
                    })
                    .unwrap_or(80);
                if self.columns != columns {
                    self.previous_line.clear();
                }
                self.columns = columns;
                self.width_checked = Some(now);
            }
            // Signals can terminate explicit indexing without running Drop.
            // Preserve cursor visibility; interactive sessions own that state.
            self.visible = true;
            let mut line = String::new();
            format_terminal_progress(
                &mut line,
                progress,
                self.color,
                self.unicode,
                self.columns,
                now.duration_since(self.stage_started),
                self.tick,
            );
            // Overwrite the changed text before clearing the old tail, and
            // submit one complete frame without a standalone clear-line write.
            if warned || self.previous_line.is_empty() {
                self.frame.push('\r');
                self.frame.push_str(&line);
                self.frame.push_str("\x1b[K");
            } else if line != self.previous_line {
                if let Some((byte, column)) = terminal_change_start(&self.previous_line, &line) {
                    let _ = write!(self.frame, "\x1b[{}G{}\x1b[K", column + 1, &line[byte..]);
                } else {
                    self.frame.push('\r');
                    self.frame.push_str(&line);
                    self.frame.push_str("\x1b[K");
                }
            }
            self.previous_line = line;
            self.tick = self.tick.wrapping_add(1);
        }
        if !self.frame.is_empty() {
            self.writer.write_all(self.frame.as_bytes())?;
            self.writer.flush()?;
        }
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        if self.visible {
            self.visible = false;
            self.writer.write_all(b"\r\x1b[K")?;
            self.writer.flush()?;
        }
        Ok(())
    }
}

impl Drop for TerminalOutput {
    fn drop(&mut self) {
        let _ = self.finish();
    }
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

fn green(value: &str, color: bool) -> String {
    crate::theme::StatusTheme { color }.index_accent(value)
}

fn stage_record(progress: &IndexProgress, color: bool, unicode: bool) -> String {
    let stage = TerminalStage::of(progress);
    let detail = match stage {
        // This counter is pending candidates (the original batch on retry),
        // not files scanned. Do not label it as a scan size.
        TerminalStage::Scanning | TerminalStage::Finalizing => None,
        TerminalStage::Model => progress
            .embedding
            .as_ref()
            .and_then(|embedding| embedding.model.as_deref())
            .map(|model| clean(model, false)),
        TerminalStage::Indexing => progress.files_total.map(|total| {
            let indexed = progress.files_indexed.unwrap_or(0).min(total);
            let failures = progress
                .files_failed
                .filter(|count| *count > 0)
                .map_or_else(String::new, |count| format!(" · {count} failed"));
            format!("{indexed}/{total} files{failures}")
        }),
    };
    let glyph = green(if unicode { "◆" } else { "*" }, color);
    match detail {
        Some(detail) => format!("{glyph} {:<18}  {detail}", stage.label()),
        None => format!("{glyph} {}", stage.label()),
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
    line: &mut String,
    progress: &IndexProgress,
    color: bool,
    unicode: bool,
    columns: usize,
    elapsed: Duration,
    tick: usize,
) {
    // Keep arbitrary diagnostics in durable output. Budget ambiguous Unicode
    // diamonds/blocks as two cells so CJK width preferences cannot wrap a live
    // row and leave stale fragments behind.
    let available = columns.saturating_sub(1);
    if available == 0 {
        return;
    }
    let stage = Stage::of(progress);
    let mut percent = None;
    let mut suffix = match stage {
        Stage::Indexing if progress.files_total.is_some_and(|total| total > 0) => {
            let total = progress.files_total.unwrap_or(0);
            let indexed = progress.files_indexed.unwrap_or(0).min(total);
            // Round ordinary percentages, but never claim 100% while files are
            // still pending. The bar itself rounds down to a whole cell.
            let value =
                usize::try_from((indexed as u128 * 100 + total as u128 / 2) / total as u128)
                    .unwrap_or(100)
                    .min(if indexed < total { 99 } else { 100 });
            percent = Some(value);
            format!("{value:>3}%  {indexed}/{total}")
        }
        Stage::FilesProcessed => {
            let total = progress.files_total.unwrap_or(0);
            format!("{total}/{total}")
        }
        Stage::Downloading => {
            let embedding = progress.embedding.as_ref();
            let downloaded = embedding
                .and_then(|embedding| embedding.downloaded_bytes)
                .unwrap_or(0);
            if let Some(total) = embedding
                .and_then(|embedding| embedding.total_bytes)
                .filter(|total| *total > 0)
            {
                let value =
                    usize::try_from(u128::from(downloaded.min(total)) * 100 / u128::from(total))
                        .unwrap_or(100);
                percent = Some(value);
                format!("{value:>3}%  {}/{}", bytes(downloaded), bytes(total))
            } else {
                format!("{} received", bytes(downloaded))
            }
        }
        _ => format!("{:.1}s elapsed", elapsed.as_secs_f64()),
    };
    if let Some(failed) = progress.files_failed.filter(|count| *count > 0) {
        let _ = write!(suffix, "  {failed} failed");
    }
    let label = TerminalStage::of(progress).label();
    let glyph = if unicode { "◆" } else { "*" };
    let glyph_width = if unicode { 2 } else { 1 };
    let prefix_width = glyph_width + 1 + 18 + 2;
    let bar_width =
        (available.saturating_sub(prefix_width + suffix.len() + 2) / glyph_width).min(20);
    if bar_width >= 4 {
        let _ = write!(line, "{} {label:<18}  ", green(glyph, color));
        if let Some(percent) = percent {
            line.push_str(&gradient_bar(
                bar_width * percent / 100,
                bar_width,
                color,
                unicode,
            ));
        } else {
            line.push_str(&activity_bar(bar_width, tick, color, unicode));
        }
        let _ = write!(line, "  {suffix}");
    } else {
        // Narrow terminals progressively drop the bar and counts. Reserve the
        // final column so terminals never wrap a repeatedly overwritten line.
        let compact = if let Some(percent) = percent {
            format!("{label} {percent}%")
        } else {
            format!("{label} {:.1}s", elapsed.as_secs_f64())
        };
        if available > compact.len() + glyph_width {
            let _ = write!(line, "{} {compact}", green(glyph, color));
        } else if available >= compact.len() {
            line.push_str(&compact);
        } else {
            line.push_str(&label[..label.len().min(available)]);
        }
    }
}

fn activity_bar(width: usize, tick: usize, color: bool, unicode: bool) -> String {
    let block = width.min(4);
    let travel = width - block;
    let position = if travel == 0 {
        0
    } else {
        let step = tick % (travel * 2);
        if step <= travel {
            step
        } else {
            travel * 2 - step
        }
    };
    let empty = if unicode { "░" } else { "-" };
    let lead = empty.repeat(position);
    let body = gradient_bar(block, width - position, color, unicode);
    if color {
        format!("\x1b[2m{lead}\x1b[0m{body}")
    } else {
        lead + &body
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
    let label = match Stage::of(progress) {
        Stage::Scanning => "Scanning",
        Stage::FilesProcessed => "Files processed",
        Stage::Optimizing => "Optimizing index",
        Stage::Saving => "Saving index",
        Stage::Finishing => "Finishing index",
        _ => "Indexing",
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
    if let Some(detail) = &progress.detail
        && detail != DETAIL_FINALIZING_INDEX
        && detail != DETAIL_SAVING_INDEX
        && detail != DETAIL_INDEXING_COMPLETE
    {
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
    use unicode_width::UnicodeWidthChar;
    use zg_engine::api::index::progress::IndexEmbeddingProgress;
    #[derive(Clone, Default)]
    struct Capture(Arc<(Mutex<Vec<Vec<u8>>>, Condvar)>);

    impl Capture {
        fn frames(&self) -> Vec<Vec<u8>> {
            self.0.0.lock().expect("capture").clone()
        }

        fn text(&self) -> String {
            String::from_utf8(self.frames().concat()).expect("UTF-8")
        }

        fn wait_for_frames(&self, count: usize) {
            let (frames, _) = self
                .0
                .1
                .wait_timeout_while(
                    self.0.0.lock().expect("capture"),
                    Duration::from_secs(3),
                    |frames| frames.len() < count,
                )
                .expect("capture wait");
            assert!(
                frames.len() >= count,
                "renderer did not produce {count} frames"
            );
        }
    }

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.0.lock().expect("capture").push(bytes.to_vec());
            self.0.1.notify_all();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn indexing(indexed: usize, total: usize) -> IndexProgress {
        IndexProgress {
            phase: IndexProgressPhase::Indexing,
            files_total: Some(total),
            files_indexed: Some(indexed),
            files_failed: None,
            detail: None,
            embedding: None,
        }
    }

    // Replay the single-row CSI operations emitted by the progress renderer.
    fn replay_line(frames: &[Vec<u8>], cjk: bool) -> String {
        let mut cells = vec![' '; 100];
        let mut column = 0;
        for frame in frames {
            let text = std::str::from_utf8(frame).expect("UTF-8 frame");
            let mut characters = text.chars();
            while let Some(character) = characters.next() {
                match character {
                    '\r' => column = 0,
                    '\x1b' => {
                        assert_eq!(characters.next(), Some('['));
                        let mut parameters = String::new();
                        let command = loop {
                            let code = characters.next().expect("complete CSI sequence");
                            if ('@'..='~').contains(&code) {
                                break code;
                            }
                            parameters.push(code);
                        };
                        match command {
                            'm' => {}
                            'G' => column = parameters.parse::<usize>().expect("cursor column") - 1,
                            'K' => cells[column..].fill(' '),
                            _ => panic!("unexpected CSI command: {command}"),
                        }
                    }
                    value => {
                        let width = if cjk {
                            value.width_cjk()
                        } else {
                            value.width()
                        }
                        .expect("printable progress character");
                        if width > 0 {
                            cells[column..column + width].fill(' ');
                            cells[column] = value;
                            column += width;
                        }
                    }
                }
            }
        }
        cells.into_iter().collect::<String>().trim_end().to_owned()
    }

    fn queued_display() -> IndexProgressDisplay {
        // Consume no events until the test inspects them, so every report lands
        // in the same frame without depending on renderer thread scheduling.
        IndexProgressDisplay {
            state: ProgressState::Terminal(Arc::new((
                Mutex::new(PendingProgress::default()),
                Condvar::new(),
            ))),
            worker: Mutex::new(None),
        }
    }

    fn latest_pending(display: &IndexProgressDisplay) -> IndexProgress {
        let ProgressState::Terminal(state) = &display.state else {
            panic!("expected terminal progress");
        };
        state
            .0
            .lock()
            .expect("pending progress")
            .latest
            .clone()
            .expect("queued progress")
    }

    #[test]
    fn pending_counts_survive_embedding_and_partial_events() {
        let display = queued_display();
        let reporter = display.reporter();
        reporter.report(IndexProgress {
            files_failed: Some(2),
            detail: Some("reading src/first.rs".into()),
            ..indexing(25, 100)
        });
        let partial = IndexProgress {
            phase: IndexProgressPhase::Indexing,
            files_total: None,
            files_indexed: None,
            files_failed: None,
            detail: None,
            embedding: None,
        };
        reporter.report(IndexProgress {
            embedding: Some(IndexEmbeddingProgress {
                stage: Some(IndexEmbeddingStage::Preparing),
                ..IndexEmbeddingProgress::default()
            }),
            ..partial.clone()
        });
        let model = latest_pending(&display);
        assert_eq!(
            (model.files_total, model.files_indexed, model.files_failed),
            (Some(100), Some(25), Some(2))
        );
        assert!(model.detail.is_none());
        assert!(Stage::of(&model) == Stage::Preparing);

        reporter.report(partial);
        let latest = latest_pending(&display);
        assert_eq!(
            (
                latest.files_total,
                latest.files_indexed,
                latest.files_failed
            ),
            (Some(100), Some(25), Some(2))
        );
        assert!(latest.detail.is_none());
        assert!(latest.embedding.is_none());
        assert!(Stage::of(&latest) == Stage::Indexing);
        let mut line = String::new();
        format_terminal_progress(&mut line, &latest, false, true, 100, Duration::ZERO, 0);
        assert!(line.contains("25/100"), "lost pending counts: {line}");
    }

    #[test]
    fn pending_split_counts_keep_explicit_zero_and_the_latest_stage() {
        let display = queued_display();
        let reporter = display.reporter();
        let partial = IndexProgress {
            phase: IndexProgressPhase::Indexing,
            files_total: None,
            files_indexed: None,
            files_failed: None,
            detail: None,
            embedding: None,
        };
        reporter.report(IndexProgress {
            files_total: Some(100),
            ..partial.clone()
        });
        reporter.report(IndexProgress {
            files_indexed: Some(42),
            ..partial.clone()
        });
        reporter.report(IndexProgress {
            files_failed: Some(3),
            detail: Some(DETAIL_SAVING_INDEX.into()),
            ..partial.clone()
        });
        let saving = latest_pending(&display);
        assert_eq!(
            (
                saving.files_total,
                saving.files_indexed,
                saving.files_failed
            ),
            (Some(100), Some(42), Some(3))
        );
        assert!(Stage::of(&saving) == Stage::Saving);

        reporter.report(IndexProgress {
            files_total: Some(0),
            files_failed: Some(0),
            ..partial.clone()
        });
        let reset = latest_pending(&display);
        assert_eq!(
            (reset.files_total, reset.files_indexed, reset.files_failed),
            (Some(0), Some(42), Some(0))
        );
        assert!(reset.detail.is_none());
        assert!(Stage::of(&reset) == Stage::Indexing);

        reporter.report(IndexProgress {
            phase: IndexProgressPhase::Done,
            files_indexed: Some(0),
            ..partial
        });
        let done = latest_pending(&display);
        assert_eq!(
            (done.files_total, done.files_indexed, done.files_failed),
            (Some(0), Some(0), Some(0))
        );
        assert_eq!(done.phase, IndexProgressPhase::Done);
        assert!(done.detail.is_none());
        assert!(done.embedding.is_none());
        assert!(Stage::of(&done) == Stage::Finishing);
    }

    #[test]
    fn pending_scan_reset_survives_coalescing_after_a_rendered_frame() {
        let display = queued_display();
        let reporter = display.reporter();
        let ProgressState::Terminal(state) = &display.state else {
            panic!("expected terminal progress");
        };
        let capture = Capture::default();
        let now = Instant::now();
        let mut output = TerminalOutput {
            writer: Box::new(capture.clone()),
            color: false,
            unicode: true,
            latest: None,
            stage: None,
            stage_started: now,
            frame: String::new(),
            previous_line: String::new(),
            visible: false,
            columns: 100,
            width_checked: Some(now),
            tick: 0,
        };
        let mut render_pending = || {
            let mut pending = state.0.lock().expect("pending progress");
            let latest = pending.latest.take();
            let transitions = std::mem::take(&mut pending.transitions);
            drop(pending);
            output
                .render(transitions, latest, Vec::new(), now, false)
                .expect("render queued progress");
            output.latest.clone().expect("rendered progress")
        };
        reporter.report(indexing(50, 100));
        render_pending();
        let partial = IndexProgress {
            phase: IndexProgressPhase::Indexing,
            files_total: None,
            files_indexed: None,
            files_failed: Some(4),
            detail: None,
            embedding: None,
        };
        reporter.report(partial.clone());
        let inherited = render_pending();
        assert_eq!(
            (
                inherited.files_total,
                inherited.files_indexed,
                inherited.files_failed
            ),
            (Some(100), Some(50), Some(4))
        );

        reporter.report(IndexProgress {
            phase: IndexProgressPhase::Scanning,
            files_failed: None,
            ..partial.clone()
        });
        reporter.report(IndexProgress {
            files_failed: None,
            ..partial
        });
        let reset = render_pending();
        assert_eq!(
            (reset.files_total, reset.files_indexed, reset.files_failed),
            (None, None, None)
        );
        assert!(Stage::of(&reset) == Stage::Indexing);
        let last = &output.previous_line;
        assert!(!last.contains("50/100"));
        assert!(!last.contains("failed"));
    }

    #[test]
    fn fast_stages_survive_coalescing_and_finish_without_a_transient_animation() {
        let display = queued_display();
        let reporter = display.reporter();
        reporter.report(IndexProgress {
            phase: IndexProgressPhase::Scanning,
            ..indexing(0, 2)
        });
        reporter.report(indexing(0, 2));
        for stage in [
            IndexEmbeddingStage::Preparing,
            IndexEmbeddingStage::Downloading,
            IndexEmbeddingStage::Ready,
        ] {
            reporter.report(IndexProgress {
                embedding: Some(IndexEmbeddingProgress {
                    stage: Some(stage),
                    model: Some("local/test-model".into()),
                    ..IndexEmbeddingProgress::default()
                }),
                ..indexing(0, 2)
            });
        }
        reporter.report(indexing(1, 2));
        reporter.report(indexing(2, 2));
        for detail in [DETAIL_FINALIZING_INDEX, DETAIL_SAVING_INDEX] {
            reporter.report(IndexProgress {
                phase: IndexProgressPhase::Finalizing,
                detail: Some(detail.into()),
                ..indexing(2, 2)
            });
        }
        reporter.report(IndexProgress {
            phase: IndexProgressPhase::Done,
            ..indexing(2, 2)
        });
        let ProgressState::Terminal(state) = &display.state else {
            panic!("expected terminal progress");
        };
        let mut pending = state.0.lock().expect("pending progress");
        let capture = Capture::default();
        let now = Instant::now();
        let mut output = TerminalOutput {
            writer: Box::new(capture.clone()),
            color: false,
            unicode: true,
            latest: None,
            stage: None,
            stage_started: now,
            frame: String::new(),
            previous_line: String::new(),
            visible: false,
            columns: 80,
            width_checked: Some(now),
            tick: 0,
        };
        output
            .render(
                std::mem::take(&mut pending.transitions),
                pending.latest.take(),
                Vec::new(),
                now,
                true,
            )
            .expect("finish fast operation");
        output.finish().expect("cleanup");
        let text = capture.text().replace('\r', "").replace("\x1b[K", "");
        assert_eq!(
            text,
            concat!(
                "◆ Scanning workspace\n",
                "◆ Preparing model     local/test-model\n",
                "◆ Indexing files      2/2 files\n",
            )
        );
        assert_eq!(capture.frames().len(), 1, "one atomic history update");
        assert!(
            !text.contains("Index ready"),
            "only the CLI result can claim readiness"
        );
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
    fn activity_stays_visible_without_claiming_a_percentage() {
        assert_eq!(activity_bar(8, 0, false, true), "████░░░░");
        assert_eq!(activity_bar(8, 4, false, true), "░░░░████");
        assert_eq!(activity_bar(8, 5, false, true), "░░░████░");
        assert_eq!(activity_bar(8, 8, false, true), "████░░░░");
        for width in 1..30 {
            for tick in 0..100 {
                let bar = activity_bar(width, tick, false, true);
                assert_eq!(bar.chars().count(), width);
                assert!(bar.contains('█'), "the moving block must never disappear");
            }
        }
    }

    #[test]
    fn live_output_reserves_the_last_terminal_column() {
        for unicode in [false, true] {
            for columns in 0..160 {
                for progress in [
                    indexing(usize::MAX - 1, usize::MAX),
                    IndexProgress {
                        detail: Some(DETAIL_FINALIZING_INDEX.into()),
                        ..indexing(10, 10)
                    },
                    IndexProgress {
                        phase: IndexProgressPhase::Scanning,
                        ..indexing(0, 0)
                    },
                ] {
                    let mut line = String::new();
                    format_terminal_progress(
                        &mut line,
                        &progress,
                        false,
                        unicode,
                        columns,
                        Duration::from_secs(100_000),
                        5,
                    );
                    assert!(
                        line.chars()
                            .map(|value| value.width_cjk().unwrap_or(0))
                            .sum::<usize>()
                            <= columns.saturating_sub(1),
                        "wrapped {columns}-column terminal: {line:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn file_progress_does_not_round_up_to_complete() {
        let mut line = String::new();
        format_terminal_progress(
            &mut line,
            &indexing(999, 1000),
            false,
            true,
            100,
            Duration::ZERO,
            0,
        );
        assert!(line.contains("99%"));
        assert!(!line.contains("100%"));
        assert!(line.contains('░'));
        line.clear();
        format_terminal_progress(
            &mut line,
            &indexing(1000, 1000),
            false,
            true,
            100,
            Duration::ZERO,
            0,
        );
        assert!(line.contains("Indexing files"));
        assert!(line.contains("1000/1000"));
        assert!(!line.contains("100%"));
        line.clear();
        format_terminal_progress(
            &mut line,
            &indexing(18_647, 36_718),
            false,
            true,
            100,
            Duration::ZERO,
            0,
        );
        assert!(line.contains("51%"));
    }

    #[test]
    fn indexing_updates_preserve_counts_with_both_width_policies() {
        for color in [false, true] {
            for unicode in [false, true] {
                let capture = Capture::default();
                let now = Instant::now();
                let mut output = TerminalOutput {
                    writer: Box::new(capture.clone()),
                    color,
                    unicode,
                    latest: None,
                    stage: None,
                    stage_started: now,
                    frame: String::new(),
                    previous_line: String::new(),
                    visible: false,
                    columns: 100,
                    width_checked: Some(now),
                    tick: 0,
                };
                output
                    .render(
                        Vec::new(),
                        Some(indexing(892, 20_936)),
                        Vec::new(),
                        now,
                        false,
                    )
                    .expect("first progress frame");
                output
                    .render(
                        Vec::new(),
                        Some(indexing(893, 20_936)),
                        Vec::new(),
                        now + FRAME_INTERVAL,
                        false,
                    )
                    .expect("next progress frame");
                let frames = capture.frames();
                assert_eq!(frames.len(), 2);
                let reference = format!("\r{}\x1b[K", output.previous_line).into_bytes();
                for cjk in [false, true] {
                    let actual = replay_line(&frames, cjk);
                    assert!(
                        actual.contains("893/20936"),
                        "color={color}, unicode={unicode}, cjk={cjk}: {actual:?}"
                    );
                    assert_eq!(actual, replay_line(std::slice::from_ref(&reference), cjk));
                }
                let frame = String::from_utf8_lossy(&frames[1]);
                if unicode {
                    assert!(frame.starts_with('\r'), "{frame:?}");
                    assert!(frame.contains("Indexing files"), "{frame:?}");
                } else {
                    assert!(frame.starts_with("\x1b["), "{frame:?}");
                    assert!(frame.ends_with("3/20936\x1b[K"), "{frame:?}");
                    assert!(!frame.contains("Indexing files"), "{frame:?}");
                    assert!(!frame.contains(['#', '-']), "{frame:?}");
                    assert!(!frame.contains('%'), "{frame:?}");
                }
            }
        }
    }

    #[test]
    fn a_blocked_terminal_write_does_not_block_progress_producers() {
        struct BlockedWriter {
            gate: Arc<(Mutex<bool>, Condvar)>,
            entered: std::sync::mpsc::Sender<()>,
        }
        impl Write for BlockedWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let _ = self.entered.send(());
                let (gate, wake) = &*self.gate;
                let mut released = gate.lock().expect("write gate");
                while !*released {
                    released = wake.wait(released).expect("write wait");
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (entered, started) = std::sync::mpsc::channel();
        let display = IndexProgressDisplay::terminal(
            BlockedWriter {
                gate: Arc::clone(&gate),
                entered,
            },
            ColorMode::Never,
            true,
        );
        let reporter = display.reporter();
        reporter.report(indexing(0, 10));
        started
            .recv_timeout(Duration::from_secs(3))
            .expect("renderer entered writer");
        let (sender, received) = std::sync::mpsc::channel();
        let producer = thread::spawn(move || {
            reporter.report(indexing(1, 10));
            let _ = sender.send(());
        });
        let independent = received.recv_timeout(Duration::from_secs(1)).is_ok();
        *gate.0.lock().expect("release writer") = true;
        gate.1.notify_one();
        producer.join().expect("producer");
        display.finish();
        assert!(independent, "terminal I/O held the producer lock");
    }

    #[test]
    fn plain_bursts_are_throttled_but_each_tail_stage_is_logged() {
        let capture = Capture::default();
        let display = IndexProgressDisplay::new(capture.clone(), false, ColorMode::Never);
        let reporter = display.reporter();
        for indexed in 0..10_000 {
            reporter.report(indexing(indexed, 10_000));
        }
        for detail in [DETAIL_FINALIZING_INDEX, DETAIL_SAVING_INDEX] {
            reporter.report(IndexProgress {
                phase: IndexProgressPhase::Finalizing,
                detail: Some(detail.into()),
                ..indexing(10_000, 10_000)
            });
        }
        reporter.report(IndexProgress {
            phase: IndexProgressPhase::Done,
            ..indexing(10_000, 10_000)
        });
        drop(display);
        assert_eq!(capture.frames().len(), 4);
        assert_eq!(
            capture.text(),
            "Indexing: 0/10000 files\nOptimizing index: 10000/10000 files\nSaving index: 10000/10000 files\nFinishing index: 10000/10000 files\n"
        );
        assert!(!capture.text().contains('\x1b'));
    }

    #[test]
    fn terminal_coalesces_bursts_and_clears_line_on_drop() {
        let capture = Capture::default();
        let display = IndexProgressDisplay::terminal(capture.clone(), ColorMode::Never, true);
        let reporter = display.reporter();
        reporter.report(indexing(0, 10_000));
        capture.wait_for_frames(1);
        let started = Instant::now();
        for indexed in 1..10_000 {
            reporter.report(indexing(indexed, 10_000));
        }
        drop(display);
        let frames = capture.frames();
        let maximum = started.elapsed().as_millis() / FRAME_INTERVAL.as_millis() + 3;
        assert!(
            frames.len() as u128 <= maximum,
            "unthrottled frame count: {}",
            frames.len()
        );
        assert_eq!(frames.last().expect("cleanup"), b"\r\x1b[K");
        let text = capture.text();
        assert!(!text.contains("\x1b[?25l"));
        assert!(!text.contains("\x1b[?25h"));
        assert!(!text.contains("\x1b[2K"));
        assert!(String::from_utf8_lossy(&frames[0]).contains("Indexing files"));
        for frame in &frames[..frames.len() - 1] {
            let frame = String::from_utf8_lossy(frame);
            assert!(frame.ends_with("\x1b[K"));
            assert!(frame.starts_with('\r') || frame.starts_with("\x1b["));
        }
        reporter.report(indexing(10_000, 10_000));
        assert_eq!(
            capture.frames(),
            frames,
            "late events must not recreate the live line"
        );
    }

    #[test]
    fn heartbeat_animates_optimization_without_producer_events() {
        let capture = Capture::default();
        let display = IndexProgressDisplay::terminal(capture.clone(), ColorMode::Never, true);
        display.reporter().report(IndexProgress {
            phase: IndexProgressPhase::Finalizing,
            ..indexing(10, 10)
        });
        capture.wait_for_frames(3);
        display.finish();
        display.finish();
        let frames = capture.frames();
        assert_ne!(frames[1], frames[2]);
        let text = capture.text();
        assert!(text.contains("Finalizing index"));
        assert!(text.contains("elapsed"));
        assert!(!text.contains("100%"));
        assert!(!text.contains("\x1b[?25l"));
        assert!(!text.contains("\x1b[?25h"));
    }

    #[test]
    fn finalizing_phase_identifies_optimization_and_saving() {
        for detail in [
            None,
            Some(DETAIL_FINALIZING_INDEX),
            Some(DETAIL_SAVING_INDEX),
        ] {
            let progress = IndexProgress {
                phase: IndexProgressPhase::Finalizing,
                detail: detail.map(str::to_owned),
                ..indexing(10, 10)
            };
            let expected = if detail == Some(DETAIL_SAVING_INDEX) {
                Stage::Saving
            } else {
                Stage::Optimizing
            };
            assert!(Stage::of(&progress) == expected);
            let mut line = String::new();
            format_terminal_progress(
                &mut line,
                &progress,
                false,
                true,
                100,
                Duration::from_secs(3),
                0,
            );
            assert!(line.contains("Finalizing index"));
            assert!(line.contains("3.0s elapsed"));
            assert!(!line.contains("100%"));
        }
    }

    #[test]
    fn pipeline_done_stays_finishing_until_cli_cleanup_completes() {
        let capture = Capture::default();
        let display = IndexProgressDisplay::terminal(capture.clone(), ColorMode::Never, true);
        display.reporter().report(IndexProgress {
            phase: IndexProgressPhase::Done,
            ..indexing(9, 10)
        });
        capture.wait_for_frames(2);
        display.finish();
        let text = capture.text();
        assert!(text.contains("Finalizing index"));
        assert!(!text.contains("Index ready"));
        assert!(!text.contains("100%"));
    }

    #[test]
    fn warnings_are_durable_complete_and_survive_coalescing() {
        let warning = "Transformers cuda embedding initialization failed (Unable to configure CUDA ONNX embedding model), falling back to CPU.";
        for terminal in [false, true] {
            let capture = Capture::default();
            let display = if terminal {
                IndexProgressDisplay::terminal(capture.clone(), ColorMode::Never, true)
            } else {
                IndexProgressDisplay::new(capture.clone(), false, ColorMode::Never)
            };
            let reporter = display.reporter();
            let progress = IndexProgress {
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
            };
            reporter.report(progress.clone());
            reporter.report(progress);
            reporter.report(indexing(3, 8));
            drop(display);
            assert_eq!(capture.text().matches(warning).count(), 2);
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
            format!("Finishing index — {sanitized}")
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
