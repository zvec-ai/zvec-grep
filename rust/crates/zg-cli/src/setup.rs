//! Terminal index setup, with independent selection positions for both model tabs.

mod credential;
pub use credential::prompt_setup_credential;

use std::{
    io::{self, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crossterm::{
    cursor::{Hide, MoveToColumn, MoveUp, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    queue,
    terminal::{self, Clear, ClearType},
};
use tokio_util::sync::CancellationToken;
use zg_engine::{
    api::index::setup::{EmbeddingChoice, IndexSetup},
    authorization::IndexAuthorization,
};

use crate::ColorMode;

struct TerminalSession {
    was_raw: bool,
}

impl TerminalSession {
    fn new() -> io::Result<Self> {
        let was_raw = terminal::is_raw_mode_enabled()?;
        terminal::enable_raw_mode()?;
        let session = Self { was_raw };
        crossterm::execute!(io::stderr(), Hide)?;
        Ok(session)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = crossterm::execute!(io::stderr(), Show);
        if !self.was_raw {
            let _ = terminal::disable_raw_mode();
        }
    }
}

fn cancelled(key: KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

fn clean(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[derive(Default)]
struct Screen {
    lines: u16,
}

impl Screen {
    fn rewind(&self, output: &mut impl Write) -> io::Result<()> {
        queue!(output, MoveToColumn(0))?;
        if self.lines > 0 {
            queue!(output, MoveUp(self.lines))?;
        }
        queue!(output, Clear(ClearType::FromCursorDown))
    }

    fn clear(&mut self) -> io::Result<()> {
        let mut output = io::stderr().lock();
        self.rewind(&mut output)?;
        output.flush()?;
        self.lines = 0;
        Ok(())
    }

    fn draw(&mut self, lines: &[String], color: ColorMode) -> io::Result<()> {
        let mut output = io::stderr().lock();
        self.rewind(&mut output)?;
        let columns = terminal::size()
            .map_or(80, |(columns, _)| columns)
            .saturating_sub(1);
        let use_color = color == ColorMode::Always
            || (color == ColorMode::Auto && std::env::var_os("NO_COLOR").is_none());
        for line in lines {
            let line = clean(line)
                .chars()
                .take(usize::from(columns))
                .collect::<String>();
            if use_color && line.trim_start().starts_with('›') {
                write!(output, "\x1b[38;2;134;239;172m{line}\x1b[0m\r\n")?;
            } else {
                write!(output, "{line}\r\n")?;
            }
        }
        output.flush()?;
        self.lines = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        Ok(())
    }
}

pub struct ModelSelection {
    setup: IndexSetup,
    tab: usize,
    positions: [usize; 2],
}

#[derive(Debug, Eq, PartialEq)]
enum SelectionResult {
    Selected(EmbeddingChoice),
    Cancelled,
}

impl ModelSelection {
    #[must_use]
    pub fn new(setup: &IndexSetup) -> Self {
        let mut selection = Self {
            setup: setup.clone(),
            tab: 0,
            positions: [0, 0],
        };
        if let Some(default) = setup.models.iter().find(|model| {
            Some(&model.reference) == setup.default_model.as_ref() && model.available()
        }) {
            selection.tab = usize::from(!default.local);
            selection.positions[selection.tab] = selection
                .choices()
                .iter()
                .position(|model| model.reference == default.reference)
                .unwrap_or(0);
        }
        selection
    }

    /// Refreshes a provider's credential status without resetting either tab.
    pub fn set_provider_credential_available(&mut self, provider: &str) {
        for model in &mut self.setup.models {
            if !model.local && model.reference.split('/').next() == Some(provider) {
                model.credential_available = true;
            }
        }
    }

    fn choices(&self) -> Vec<&EmbeddingChoice> {
        self.setup
            .models
            .iter()
            .filter(|model| model.local == (self.tab == 0))
            .collect()
    }

    fn key(&mut self, key: KeyEvent) -> Option<SelectionResult> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if cancelled(key) {
            return Some(SelectionResult::Cancelled);
        }
        let count = self.choices().len();
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                self.tab = 1 - self.tab;
            }
            KeyCode::Up if count > 0 => {
                self.positions[self.tab] = (self.positions[self.tab] + count - 1) % count;
            }
            KeyCode::Down if count > 0 => {
                self.positions[self.tab] = (self.positions[self.tab] + 1) % count;
            }
            KeyCode::Enter => {
                if let Some(model) = self.choices().get(self.positions[self.tab]) {
                    return Some(SelectionResult::Selected((*model).clone()));
                }
            }
            _ => {}
        }
        None
    }

    fn lines(&self, root: &Path, height: u16) -> Vec<String> {
        let mut lines = vec![
            format!(
                "No index found in {}.",
                clean(&crate::output_path(root).display().to_string())
            ),
            String::new(),
            "Select an embedding model to build zg index:".into(),
            String::new(),
            if self.tab == 0 {
                "  [ Local models ]    Remote models"
            } else {
                "    Local models    [ Remote models ]"
            }
            .into(),
            String::new(),
        ];
        let choices = self.choices();
        let visible = usize::from(height.saturating_sub(11)).clamp(1, 12);
        let active = self.positions[self.tab];
        let start = active.saturating_sub(visible - 1);
        let width = choices
            .iter()
            .map(|model| model.reference.len())
            .max()
            .unwrap_or(0);
        for (index, model) in choices.iter().enumerate().skip(start).take(visible) {
            let status = if model.local {
                if model.downloaded {
                    "Downloaded"
                } else {
                    "Download required"
                }
                .to_owned()
                    + &model
                        .download_bytes
                        .filter(|_| !model.downloaded)
                        .map_or_else(String::new, |bytes| {
                            format!(" · {} MiB", bytes.div_ceil(1024 * 1024))
                        })
            } else {
                if model.credential_available {
                    "Credential available"
                } else {
                    "Missing API credential"
                }
                .to_owned()
            };
            lines.push(format!(
                "  {} {:width$}  {status}",
                if index == active { "›" } else { " " },
                model.reference
            ));
        }
        if choices.is_empty() {
            lines.push("    No supported models".into());
        }
        if choices.len() > visible {
            lines.push(format!("  {}/{} models", active + 1, choices.len()));
        }
        lines.push(String::new());
        lines.push(format!(
            "  ↑/↓ Select · ←/→ or Tab Switch · Enter {} · Esc Exit",
            if self.tab == 0 { "Build" } else { "Continue" }
        ));
        if choices.get(active).is_some_and(|model| !model.available()) {
            lines.push("  Continue to set an API Key before building.".into());
        }
        lines
    }

    /// Preserves each tab's selection across switches and later prompts.
    /// # Errors
    /// Returns terminal setup or input/output failures.
    pub fn select(&mut self, root: &Path, color: ColorMode) -> io::Result<Option<EmbeddingChoice>> {
        let _terminal = TerminalSession::new()?;
        let mut screen = Screen::default();
        loop {
            screen.draw(
                &self.lines(root, terminal::size().map_or(24, |(_, height)| height)),
                color,
            )?;
            if let Event::Key(key) = event::read()?
                && let Some(result) = self.key(key)
            {
                screen.clear()?;
                return Ok(match result {
                    SelectionResult::Selected(model) => Some(model),
                    SelectionResult::Cancelled => None,
                });
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetupAuthorizationDecision {
    Once,
    Workspace,
    Back,
    Cancel,
}

/// Requests consent before indexing sends any workspace content.
/// # Errors
/// Returns terminal setup or input/output failures.
pub fn prompt_setup_authorization(
    target: &IndexAuthorization,
    query_after: bool,
    color: ColorMode,
) -> io::Result<SetupAuthorizationDecision> {
    let _terminal = TerminalSession::new()?;
    let mut screen = Screen::default();
    let mut active = 0;
    loop {
        let mut lines = vec![
            "Remote embedding authorization".into(),
            String::new(),
            format!(
                "  Workspace  {}",
                crate::output_path(&target.root).display()
            ),
            format!("  Model      {}", target.model),
            format!("  Endpoint   {}", target.endpoint),
            format!(
                "  Sends      {}",
                if query_after {
                    "selected workspace files and query text"
                } else {
                    "selected workspace files"
                }
            ),
            "  API charges may apply.".into(),
            String::new(),
            format!(
                "  Allow once covers {}.",
                if query_after {
                    "this build and the following query"
                } else {
                    "this background build only"
                }
            ),
            String::new(),
        ];
        for (index, choice) in [
            "Allow once",
            "Allow for this workspace",
            "Back to model selection",
        ]
        .iter()
        .enumerate()
        {
            lines.push(format!(
                "  {} {choice}",
                if index == active { "›" } else { " " }
            ));
        }
        lines.extend([
            String::new(),
            "  ↑/↓ Select · Enter Confirm · Esc Exit".into(),
        ]);
        screen.draw(&lines, color)?;
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            if cancelled(key) {
                screen.clear()?;
                return Ok(SetupAuthorizationDecision::Cancel);
            }
            match key.code {
                KeyCode::Up => active = (active + 2) % 3,
                KeyCode::Down => active = (active + 1) % 3,
                KeyCode::Enter => {
                    screen.clear()?;
                    return Ok([
                        SetupAuthorizationDecision::Once,
                        SetupAuthorizationDecision::Workspace,
                        SetupAuthorizationDecision::Back,
                    ][active]);
                }
                _ => {}
            }
        }
    }
}

/// Owns the terminal event reader while an index is built in the foreground.
pub struct IndexCancellation {
    signal: CancellationToken,
    stopped: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    _terminal: TerminalSession,
}

impl IndexCancellation {
    /// # Errors
    /// Returns a terminal setup error before any worker is started.
    pub fn new() -> io::Result<Self> {
        let terminal = TerminalSession::new()?;
        let signal = CancellationToken::new();
        let stopped = Arc::new(AtomicBool::new(false));
        let reader_signal = signal.clone();
        let reader_stopped = Arc::clone(&stopped);
        let reader = thread::spawn(move || {
            while !reader_stopped.load(Ordering::Acquire) {
                let Ok(ready) = event::poll(Duration::from_millis(100)) else {
                    reader_signal.cancel();
                    break;
                };
                if ready {
                    match event::read() {
                        Ok(Event::Key(key))
                            if key.kind != KeyEventKind::Release && cancelled(key) =>
                        {
                            reader_signal.cancel();
                            break;
                        }
                        Err(_) => {
                            reader_signal.cancel();
                            break;
                        }
                        _ => {}
                    }
                }
            }
        });
        Ok(Self {
            signal,
            stopped,
            reader: Some(reader),
            _terminal: terminal,
        })
    }

    #[must_use]
    pub fn signal(&self) -> CancellationToken {
        self.signal.clone()
    }
}

impl Drop for IndexCancellation {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> IndexSetup {
        IndexSetup {
            default_model: Some("remote/ready".into()),
            models: vec![
                EmbeddingChoice {
                    reference: "local/one".into(),
                    local: true,
                    downloaded: true,
                    configured: false,
                    credential_available: true,
                    download_bytes: None,
                },
                EmbeddingChoice {
                    reference: "local/two".into(),
                    local: true,
                    downloaded: false,
                    configured: false,
                    credential_available: true,
                    download_bytes: None,
                },
                EmbeddingChoice {
                    reference: "remote/missing".into(),
                    local: false,
                    downloaded: false,
                    configured: false,
                    credential_available: false,
                    download_bytes: None,
                },
                EmbeddingChoice {
                    reference: "remote/ready".into(),
                    local: false,
                    downloaded: false,
                    configured: true,
                    credential_available: true,
                    download_bytes: None,
                },
            ],
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn tab_positions_survive_switches_and_missing_credentials_can_confirm() {
        let setup = setup();
        let mut picker = ModelSelection::new(&setup);
        assert_eq!(picker.tab, 1);
        assert_eq!(picker.positions, [0, 1]);
        picker.key(key(KeyCode::Tab));
        picker.key(key(KeyCode::Down));
        picker.key(key(KeyCode::Right));
        assert_eq!(picker.positions, [1, 1]);
        picker.key(key(KeyCode::Up));
        let Some(SelectionResult::Selected(missing)) = picker.key(key(KeyCode::Enter)) else {
            panic!("select model to configure credentials");
        };
        assert_eq!(missing.reference, "remote/missing");
        assert!(!missing.credential_available);
        picker.set_provider_credential_available("remote");
        assert_eq!(picker.positions, [1, 0]);
        let lines = picker.lines(Path::new("/workspace"), 24).join("\n");
        assert!(!lines.contains("Missing API credential"));
        assert!(lines.contains("Credential available"));
        picker.key(key(KeyCode::Left));
        let Some(SelectionResult::Selected(chosen)) = picker.key(key(KeyCode::Enter)) else {
            panic!("selected model");
        };
        assert_eq!(chosen.reference, "local/two");
        assert_eq!(
            picker.key(key(KeyCode::Esc)),
            Some(SelectionResult::Cancelled)
        );
    }

    #[test]
    fn invalid_default_opens_local_and_scrolling_keeps_selection_visible() {
        let mut setup = setup();
        setup.default_model = Some("remote/missing".into());
        let mut picker = ModelSelection::new(&setup);
        assert_eq!(picker.tab, 0);
        picker.key(key(KeyCode::Down));
        let lines = picker.lines(Path::new("/workspace"), 12).join("\n");
        assert!(lines.contains("› local/two"));
        assert!(lines.contains("Select an embedding model to build zg index:"));
        assert!(!lines.contains("local/one"));
        assert!(lines.contains("Esc Exit"));
    }
}
