use std::str::SplitAsciiWhitespace;

use crate::utils::decode_text;

use super::FileFormat;

/// Unknown suffixes use text chunks; only suffixless files can supply interpreter hints.
pub(super) fn format(bytes: &[u8], complete: bool, extensionless: bool) -> Option<FileFormat> {
    let text = decode_text(bytes, complete)?;
    if text.is_empty()
        || text
            .chars()
            .any(|ch| ch.is_control() && !ch.is_whitespace())
    {
        return None;
    }
    if extensionless {
        Some(shebang(&text, complete).unwrap_or(FileFormat::Text))
    } else {
        Some(FileFormat::Text)
    }
}

fn shebang(text: &str, complete: bool) -> Option<FileFormat> {
    let line = match text.find(['\r', '\n']) {
        Some(end) => &text[..end],
        None if complete => text,
        None => return None,
    };
    let mut words = line.strip_prefix("#!")?.split_ascii_whitespace();
    let executable = words.next()?.rsplit('/').next()?;
    if executable != "env" {
        return command_format(executable, words);
    }
    while let Some(word) = words.next() {
        match word {
            "--" => return command_format(words.next()?, words),
            "-S" | "--split-string" | "-i" | "--ignore-environment" => {}
            word if word.contains('=') && !word.starts_with('-') => {}
            word if word.starts_with('-') => return None,
            command => return command_format(command, words),
        }
    }
    None
}

fn command_format(name: &str, words: SplitAsciiWhitespace<'_>) -> Option<FileFormat> {
    match name.rsplit('/').next()? {
        "uv" => uv_script(words),
        name => interpreter(name),
    }
}

fn uv_script(mut words: SplitAsciiWhitespace<'_>) -> Option<FileFormat> {
    // uv can also run non-Python commands; require explicit script execution.
    if words.next()? != "run" {
        return None;
    }
    let mut script = false;
    while let Some(word) = words.next() {
        let (option, inline_value) = word
            .split_once('=')
            .map_or((word, None), |(option, value)| (option, Some(value)));
        match option {
            "--script" | "-s" if inline_value.is_none() => script = true,
            "--no-project" | "--isolated" | "--no-sync" | "--locked" | "--frozen" | "--offline"
            | "--no-config" | "--no-cache" | "-n" | "--quiet" | "-q" | "--verbose" | "-v"
                if inline_value.is_none() => {}
            "--project"
            | "--directory"
            | "--cache-dir"
            | "--python"
            | "-p"
            | "--with"
            | "--with-requirements"
            | "--with-editable" => {
                // Consume values rather than mistaking them for script flags.
                let value = inline_value.or_else(|| words.next())?;
                if value.is_empty() || value.starts_with('-') {
                    return None;
                }
            }
            "--" if inline_value.is_none() => {
                if words.next().is_some() {
                    return None;
                }
                break;
            }
            _ => return None,
        }
    }
    script.then_some(FileFormat::Python)
}

fn interpreter(name: &str) -> Option<FileFormat> {
    let format = match name {
        "sh" => FileFormat::Shell,
        "bash" => FileFormat::Bash,
        "zsh" => FileFormat::Zsh,
        "fish" => FileFormat::Fish,
        "python" => FileFormat::Python,
        "node" | "nodejs" => FileFormat::JavaScript,
        "ts-node" | "ts-node-esm" => FileFormat::TypeScript,
        "groovy" => FileFormat::Groovy,
        "perl" => FileFormat::Perl,
        "ruby" => FileFormat::Ruby,
        "php" => FileFormat::Php,
        "lua" | "luajit" => FileFormat::Lua,
        "Rscript" => FileFormat::R,
        "pwsh" => FileFormat::PowerShell,
        name if name.strip_prefix("python").is_some_and(|version| {
            version
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        }) =>
        {
            FileFormat::Python
        }
        _ => return None,
    };
    Some(format)
}
