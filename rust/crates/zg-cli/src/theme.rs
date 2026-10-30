//! Shared presentation for the human-readable management commands.

use std::{
    fmt::Display,
    io::{self, Write},
    path::{Path, PathBuf},
};

use crate::{ColorMode, output_path};
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy)]
pub(crate) struct StatusTheme {
    pub color: bool,
}

impl StatusTheme {
    pub fn new(mode: ColorMode, terminal: bool) -> Self {
        Self {
            color: mode == ColorMode::Always
                || (mode == ColorMode::Auto && terminal && std::env::var_os("NO_COLOR").is_none()),
        }
    }

    fn paint(self, value: &str, code: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{value}\x1b[0m")
        } else {
            value.to_owned()
        }
    }

    pub fn label(self, value: &str) -> String {
        self.paint(value, "2")
    }
    pub fn path(self, value: &str) -> String {
        self.muted(value)
    }
    pub fn success(self, value: &str) -> String {
        self.paint(value, "38;2;111;207;145")
    }
    pub fn warning(self, value: &str) -> String {
        self.paint(value, "33")
    }
    pub fn danger(self, value: &str) -> String {
        self.paint(value, "31")
    }
    pub fn accent(value: &str) -> String {
        value.to_owned()
    }
    pub fn muted(self, value: &str) -> String {
        self.paint(value, "2")
    }
}

pub(crate) fn clean(value: &str) -> String {
    value
        .chars()
        .map(|value| if value.is_control() { ' ' } else { value })
        .collect()
}

/// Bounds a plain (not ANSI-styled) line before applying colors. Counting each
/// character conservatively also avoids splitting a wide CJK character.
pub(crate) fn truncate(value: &str, columns: usize) -> String {
    let mut used = 0;
    value
        .chars()
        .map(|value| if value.is_control() { ' ' } else { value })
        .take_while(|value| {
            used += value.width().unwrap_or(0);
            used <= columns
        })
        .collect()
}

pub(crate) fn unicode_supported() -> bool {
    !cfg!(windows) && !matches!(std::env::var("TERM").as_deref(), Ok("linux" | "dumb"))
}

pub(crate) fn columns() -> usize {
    crossterm::terminal::size()
        .ok()
        .map(|(columns, _)| usize::from(columns))
        .or_else(|| std::env::var("COLUMNS").ok()?.parse().ok())
        .unwrap_or(80)
        .saturating_sub(1)
}

pub(crate) fn write_status_field(
    writer: &mut impl Write,
    theme: StatusTheme,
    label: &str,
    values: &[String],
) -> io::Result<()> {
    for (index, value) in values.iter().enumerate() {
        if index == 0 {
            writeln!(writer, "  {}{value}", theme.label(&format!("{label:<12}")))?;
        } else {
            writeln!(writer, "              {value}")?;
        }
    }
    Ok(())
}

pub(crate) fn display_path(path: &Path) -> String {
    display_path_with_home(path, std::env::home_dir().as_deref())
}

fn display_path_with_home(path: &Path, home: Option<&Path>) -> String {
    if let Some(relative) = home.and_then(|home| relative_to_home(path, home)) {
        if relative.as_os_str().is_empty() {
            return "~".into();
        }
        return PathBuf::from("~").join(relative).display().to_string();
    }
    output_path(path).display().to_string()
}

fn relative_to_home(path: &Path, home: &Path) -> Option<PathBuf> {
    if let Ok(relative) = output_path(path).strip_prefix(output_path(home)) {
        return Some(relative.to_path_buf());
    }
    // Existing paths can use filesystem aliases, such as /var and /private/var
    // on macOS or ordinary, verbatim, and short (8.3) names on Windows.
    let path = path.canonicalize().ok()?;
    let home = home.canonicalize().ok()?;
    path.strip_prefix(home).ok().map(Path::to_path_buf)
}

pub(crate) fn storage_path(path: &Path, root: &Path) -> String {
    if let Ok(relative) = output_path(path).strip_prefix(output_path(root))
        && !relative.as_os_str().is_empty()
        && !relative
            .components()
            .any(|part| part == std::path::Component::ParentDir)
    {
        return relative.display().to_string();
    }
    display_path(path)
}

pub(crate) fn format_count(value: impl Display) -> String {
    let digits = value.to_string();
    let mut result = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            result.push(',');
        }
        result.push(digit);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_text_cannot_move_the_cursor_or_split_wide_characters() {
        assert_eq!(clean("path\n\x1b[2J"), "path  [2J");
        assert_eq!(truncate("索引 ready", 3), "索");
        assert_eq!(truncate("索引 ready", 4), "索引");
    }

    #[test]
    fn compact_paths_respect_directory_boundaries() {
        let home = Path::new("/users/alice");
        assert_eq!(display_path_with_home(home, Some(home)), "~");
        assert_eq!(
            display_path_with_home(&home.join("repo"), Some(home)),
            Path::new("~").join("repo").display().to_string()
        );
        assert_eq!(
            display_path_with_home(Path::new("/users/alice2"), Some(home)),
            "/users/alice2"
        );
        let root = home.join("repo");
        assert_eq!(
            storage_path(&root.join(".zvec-grep/index"), &root),
            Path::new(".zvec-grep/index").display().to_string()
        );
        assert_eq!(
            storage_path(Path::new("/elsewhere/storage"), &root),
            "/elsewhere/storage"
        );
        let verbatim = Path::new(r"\\?\D:\workspace\repo");
        assert_eq!(
            display_path_with_home(verbatim, None),
            if cfg!(windows) {
                r"D:\workspace\repo"
            } else {
                r"\\?\D:\workspace\repo"
            }
        );
        let long_path = format!(r"\\?\D:\{}file.txt", "folder\\".repeat(50));
        for path in [
            r"\\?\UNC\server\share\repo",
            r"\\server\share\repo",
            r"\\?\D:\workspace\NUL.txt",
            r"\\?\D:\workspace\trailing.",
            r"\\?\D:\workspace\trailing ",
            long_path.as_str(),
        ] {
            assert_eq!(display_path_with_home(Path::new(path), None), path);
        }
    }

    #[test]
    fn counts_use_english_grouping() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1234), "1,234");
        assert_eq!(format_count(u64::MAX), "18,446,744,073,709,551,615");
    }

    #[cfg(unix)]
    #[test]
    fn home_display_accepts_symlink_aliases_in_both_directions() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let home = temporary.path().join("home");
        let alias = temporary.path().join("home-alias");
        let unrelated = temporary.path().join("home-unrelated");
        std::fs::create_dir_all(home.join("repo")).expect("home and nested directory");
        std::fs::create_dir(&unrelated).expect("unrelated directory");
        std::os::unix::fs::symlink(&home, &alias).expect("home alias");
        let canonical = home.canonicalize().expect("canonical home");

        for (path, home) in [(&canonical, &alias), (&alias, &canonical)] {
            assert_eq!(display_path_with_home(path, Some(home)), "~");
            assert_eq!(
                display_path_with_home(&path.join("repo"), Some(home)),
                "~/repo"
            );
            assert_eq!(
                display_path_with_home(&unrelated, Some(home)),
                unrelated.display().to_string()
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn home_display_accepts_ordinary_and_canonical_windows_paths() {
        let ordinary = std::env::temp_dir();
        let canonical = ordinary
            .canonicalize()
            .expect("canonical temporary directory");
        assert_eq!(display_path_with_home(&ordinary, Some(&canonical)), "~");
        assert_eq!(display_path_with_home(&canonical, Some(&ordinary)), "~");
        assert_eq!(
            display_path_with_home(
                Path::new(r"\\?\D:\users\alice\repo"),
                Some(Path::new(r"D:\users\alice")),
            ),
            r"~\repo"
        );
    }
}
