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
        return interpreter(executable);
    }
    while let Some(word) = words.next() {
        match word {
            "--" => return interpreter(words.next()?.rsplit('/').next()?),
            "-S" | "--split-string" | "-i" | "--ignore-environment" => {}
            word if word.contains('=') && !word.starts_with('-') => {}
            word if word.starts_with('-') => return None,
            command => return interpreter(command.rsplit('/').next()?),
        }
    }
    None
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
