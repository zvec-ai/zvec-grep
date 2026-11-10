//! Masked terminal credential entry before remote index authorization.

use std::io;

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};

use super::{Screen, TerminalSession, cancelled};
use crate::ColorMode;

struct PasteSession;

impl Drop for PasteSession {
    fn drop(&mut self) {
        let _ = crossterm::execute!(io::stderr(), DisableBracketedPaste);
    }
}

#[derive(Default)]
struct CredentialInput {
    value: String,
    error: Option<&'static str>,
}

enum CredentialResult {
    Submit(String),
    Cancel,
}

impl CredentialInput {
    fn key(&mut self, key: KeyEvent) -> Option<CredentialResult> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if cancelled(key) {
            return Some(CredentialResult::Cancel);
        }
        match key.code {
            KeyCode::Enter => {
                let value = self.value.trim();
                if value.is_empty() {
                    self.error = Some("Enter an API Key to continue.");
                } else if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
                    self.error = Some("API Key cannot contain whitespace or control characters.");
                } else {
                    return Some(CredentialResult::Submit(value.to_owned()));
                }
            }
            KeyCode::Backspace => {
                self.value.pop();
                self.error = None;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.value.clear();
                self.error = None;
            }
            KeyCode::Char(c)
                if !c.is_control()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.value.push(c);
                self.error = None;
            }
            _ => {}
        }
        None
    }

    fn paste(&mut self, value: &str) {
        self.value.push_str(value.trim());
        self.error = None;
    }

    fn lines(&self, model: &str) -> Vec<String> {
        let provider = model.split('/').next().unwrap_or_default();
        let mut lines = vec![
            "Set API Key".into(),
            String::new(),
            format!("  Model    {model}"),
            String::new(),
            format!(
                "  ✓ API Key  [ {:_<48} ]",
                "*".repeat(self.value.chars().count().min(48))
            ),
            "  Type or paste your API Key (input is hidden).".into(),
            String::new(),
            format!("  Submitting saves the key for {provider} in the global configuration."),
            "  Remote authorization follows before any workspace data is sent.".into(),
            String::new(),
            "  Enter Save and continue · Ctrl+U Clear · Esc Cancel".into(),
        ];
        if let Some(error) = self.error {
            lines.push(format!("  × {error}"));
        }
        lines
    }
}

/// Reads a masked API Key without putting its value in terminal output.
/// # Errors
/// Returns terminal setup or input/output failures.
pub fn prompt_setup_credential(model: &str, color: ColorMode) -> io::Result<Option<String>> {
    let _terminal = TerminalSession::new()?;
    crossterm::execute!(io::stderr(), EnableBracketedPaste)?;
    let _paste = PasteSession;
    let mut input = CredentialInput::default();
    let mut screen = Screen::default();
    loop {
        screen.draw(&input.lines(model), color)?;
        match event::read()? {
            Event::Key(key) => {
                if let Some(result) = input.key(key) {
                    screen.clear()?;
                    return Ok(match result {
                        CredentialResult::Submit(value) => Some(value),
                        CredentialResult::Cancel => None,
                    });
                }
            }
            Event::Paste(value) => input.paste(&value),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn credentials_are_masked_trimmed_and_submitted_only_with_enter() {
        let mut input = CredentialInput::default();
        input.paste("  setup-test-secret\r\n");
        let lines = input.lines("qwen/text-embedding-v4").join("\n");
        assert!(!lines.contains("setup-test-secret"));
        assert!(lines.contains("*****************"));
        assert!(lines.contains("Esc Cancel"));
        let Some(CredentialResult::Submit(value)) = input.key(key(KeyCode::Enter)) else {
            panic!("submit valid key");
        };
        assert_eq!(value, "setup-test-secret");
    }

    #[test]
    fn empty_or_multiline_credentials_stay_in_the_prompt_and_can_be_corrected() {
        let mut input = CredentialInput::default();
        assert!(input.key(key(KeyCode::Enter)).is_none());
        assert!(input.error.is_some());
        input.paste("invalid\nkey");
        assert!(input.key(key(KeyCode::Enter)).is_none());
        input.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        input.key(key(KeyCode::Char('x')));
        input.key(key(KeyCode::Backspace));
        assert!(input.value.is_empty());
        input.key(key(KeyCode::Char('y')));
        assert!(matches!(
            input.key(key(KeyCode::Enter)),
            Some(CredentialResult::Submit(_))
        ));
    }

    #[test]
    fn escape_and_ctrl_c_cancel_without_submitting_a_credential() {
        for key in [
            key(KeyCode::Esc),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let mut input = CredentialInput::default();
            input.paste("setup-test-secret");
            assert!(matches!(input.key(key), Some(CredentialResult::Cancel)));
        }
    }
}
