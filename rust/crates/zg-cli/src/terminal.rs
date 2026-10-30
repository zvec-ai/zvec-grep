use std::io::{self, IsTerminal};

pub(crate) fn terminal_character_width(character: char) -> Option<usize> {
    if character.is_control() {
        Some(0)
    } else if character.is_ascii()
        || matches!(
            character,
            '│' | '·'
                | '›'
                | '↑'
                | '↓'
                | '←'
                | '→'
                | '✢'
                | '✳'
                | '✶'
                | '✻'
                | '✽'
                | '◆'
                | '█'
                | '░'
        )
    {
        Some(1)
    } else {
        None
    }
}

#[cfg(windows)]
use winapi_util::{HandleRef, console};

/// Checks stdout for ANSI support, enabling Windows virtual terminal processing.
#[must_use]
pub fn stdout_supports_ansi() -> bool {
    let terminal = io::stdout().is_terminal() && std::env::var("TERM").as_deref() != Ok("dumb");
    #[cfg(windows)]
    {
        terminal && enable_windows_ansi(&HandleRef::stdout())
    }
    #[cfg(not(windows))]
    {
        terminal
    }
}

/// Checks stderr for ANSI support, enabling Windows virtual terminal processing.
#[must_use]
pub fn stderr_supports_ansi() -> bool {
    let terminal = io::stderr().is_terminal() && std::env::var("TERM").as_deref() != Ok("dumb");
    #[cfg(windows)]
    {
        terminal && enable_windows_ansi(&HandleRef::stderr())
    }
    #[cfg(not(windows))]
    {
        terminal
    }
}

#[cfg(windows)]
fn enable_windows_ansi(handle: &HandleRef) -> bool {
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    let Ok(mode) = console::mode(handle) else {
        // IsTerminal also recognizes ANSI-capable MSYS/Cygwin PTYs.
        return true;
    };
    console::set_mode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING).is_ok()
}
