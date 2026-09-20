use std::path::PathBuf;

use thiserror::Error;
use zg_engine::api::context::ContextOptions;

use crate::parse_byte_size;

#[derive(Debug, Error)]
pub enum ManagedRgArgumentError {
    #[error("zg query --rg requires a pattern")]
    MissingPattern,
    #[error("unsupported --rg option: {0}")]
    UnsupportedOption(String),
    #[error("{0} changes rg output and cannot be used with managed --rg")]
    OutputOption(String),
    #[error("{option} requires a value")]
    MissingOptionValue { option: String },
    #[error("invalid value {value:?} for {option}")]
    InvalidOptionValue { option: String, value: String },
}

/// Parses the safe managed-ripgrep argument dialect used by the CLI.
///
/// # Errors
///
/// Returns [`ManagedRgArgumentError`] for missing patterns, invalid values,
/// unsupported options, or options that replace managed output formatting.
pub fn parse_managed_rg_args(args: &[String]) -> Result<ContextOptions, ManagedRgArgumentError> {
    let mut request = ContextOptions {
        rg: true,
        ..ContextOptions::default()
    };
    let mut index = 0;
    let mut options_finished = false;
    let mut positionals = Vec::new();
    let mut unrestricted = 0;
    while index < args.len() {
        let arg = &args[index];
        if options_finished || !arg.starts_with('-') || arg == "-" {
            positionals.push(arg.clone());
        } else if arg == "--" {
            options_finished = true;
        } else if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = long
                .split_once('=')
                .map_or((long, None), |(name, value)| (name, Some(value)));
            let name = format!("--{name}");
            if takes_value(&name) {
                let value = match inline {
                    Some(value) => value.to_owned(),
                    None => take_value(args, &mut index, &name)?,
                };
                apply_value(&name, &value, &mut request)?;
            } else if inline.is_some() {
                return Err(ManagedRgArgumentError::UnsupportedOption(arg.clone()));
            } else {
                apply_switch(&name, &mut request)?;
            }
        } else {
            for (offset, flag) in arg.char_indices().skip(1) {
                if flag == 'u' {
                    unrestricted += 1;
                    request.no_ignore = true;
                    request.hidden |= unrestricted >= 2;
                    request.rg_options.text |= unrestricted >= 3;
                    continue;
                }
                let name = short_option(flag).ok_or_else(|| {
                    let flag = format!("-{flag}");
                    if is_output_option(&flag) {
                        ManagedRgArgumentError::OutputOption(flag)
                    } else {
                        ManagedRgArgumentError::UnsupportedOption(flag)
                    }
                })?;
                if takes_value(name) {
                    let rest = &arg[offset + flag.len_utf8()..];
                    let value = if rest.is_empty() {
                        take_value(args, &mut index, name)?
                    } else {
                        rest.to_owned()
                    };
                    apply_value(name, &value, &mut request)?;
                    break;
                }
                apply_switch(name, &mut request)?;
            }
        }
        index += 1;
    }
    if request.queries.is_empty() && request.rg_options.pattern_files.is_empty() {
        if positionals.is_empty() {
            return Err(ManagedRgArgumentError::MissingPattern);
        }
        request.query = Some(positionals.remove(0));
    }
    request.rg_paths = positionals.into_iter().map(PathBuf::from).collect();
    Ok(request)
}

fn short_option(flag: char) -> Option<&'static str> {
    Some(match flag {
        'n' => "--line-number",
        'H' => "--with-filename",
        'F' => "--fixed-strings",
        'i' => "--ignore-case",
        's' => "--case-sensitive",
        'S' => "--smart-case",
        'w' => "--word-regexp",
        'x' => "--line-regexp",
        'v' => "--invert-match",
        'U' => "--multiline",
        'a' => "--text",
        'L' => "--follow",
        'e' => "--regexp",
        'f' => "--file",
        'g' => "--glob",
        't' => "--type",
        'T' => "--type-not",
        'A' => "--after-context",
        'B' => "--before-context",
        'C' => "--context",
        'm' => "--max-count",
        'j' => "--threads",
        _ => return None,
    })
}

fn apply_switch(name: &str, request: &mut ContextOptions) -> Result<(), ManagedRgArgumentError> {
    let options = &mut request.rg_options;
    match name {
        "--line-number" | "--with-filename" | "--recursive" | "--no-config" | "--no-mmap"
        | "--no-search-zip" => {}
        "--fixed-strings" => options.fixed_strings = true,
        "--no-fixed-strings" => options.fixed_strings = false,
        "--ignore-case" | "--case-sensitive" | "--smart-case" => {
            options.ignore_case = name == "--ignore-case";
            options.smart_case = name == "--smart-case";
        }
        "--word-regexp" | "--line-regexp" => {
            options.word_regexp = name == "--word-regexp";
            options.line_regexp = name == "--line-regexp";
        }
        "--invert-match" => options.invert_match = true,
        "--no-invert-match" => options.invert_match = false,
        "--multiline" => {
            options.multiline = true;
            options.stop_on_nonmatch = false;
        }
        "--no-multiline" => options.multiline = false,
        "--multiline-dotall" => options.multiline_dotall = true,
        "--no-multiline-dotall" => options.multiline_dotall = false,
        "--crlf" => options.crlf = true,
        "--no-crlf" => options.crlf = false,
        "--text" => options.text = true,
        "--no-text" => options.text = false,
        "--unicode" => options.no_unicode = false,
        "--no-unicode" => options.no_unicode = true,
        "--stop-on-nonmatch" => {
            options.stop_on_nonmatch = true;
            options.multiline = false;
        }
        "--hidden" => request.hidden = true,
        "--no-hidden" => request.hidden = false,
        "--no-ignore" => request.no_ignore = true,
        "--ignore" => request.no_ignore = false,
        "--follow" => request.follow = true,
        "--no-follow" => request.follow = false,
        "--no-ignore-dot" => options.no_ignore_dot = true,
        "--ignore-dot" => options.no_ignore_dot = false,
        "--no-ignore-files" => options.no_ignore_files = true,
        "--ignore-files" => options.no_ignore_files = false,
        "--no-ignore-global" => options.no_ignore_global = true,
        "--ignore-global" => options.no_ignore_global = false,
        "--no-ignore-parent" => options.no_ignore_parent = true,
        "--ignore-parent" => options.no_ignore_parent = false,
        "--no-ignore-vcs" => options.no_ignore_vcs = true,
        "--ignore-vcs" => options.no_ignore_vcs = false,
        "--one-file-system" => options.one_file_system = true,
        "--glob-case-insensitive" => options.glob_case_insensitive = true,
        "--no-glob-case-insensitive" => options.glob_case_insensitive = false,
        name if is_output_option(name) => {
            return Err(ManagedRgArgumentError::OutputOption(name.to_owned()));
        }
        _ => return Err(ManagedRgArgumentError::UnsupportedOption(name.to_owned())),
    }
    Ok(())
}

fn takes_value(name: &str) -> bool {
    matches!(
        name,
        "--regexp"
            | "--file"
            | "--glob"
            | "--iglob"
            | "--type"
            | "--type-not"
            | "--ignore-file"
            | "--max-depth"
            | "--max-filesize"
            | "--before-context"
            | "--after-context"
            | "--context"
            | "--max-count"
            | "--threads"
            | "--regex-size-limit"
            | "--dfa-size-limit"
            | "--engine"
    )
}

fn apply_value(
    name: &str,
    value: &str,
    request: &mut ContextOptions,
) -> Result<(), ManagedRgArgumentError> {
    if name != "--regexp" {
        non_empty(name, value)?;
    }
    match name {
        "--regexp" => request.queries.push(value.to_owned()),
        "--file" => request.rg_options.pattern_files.push(value.into()),
        "--glob" | "--iglob" => {
            request
                .rg_options
                .glob_rules
                .push(zg_engine::api::context::options::RgGlob {
                    pattern: value.to_owned(),
                    case_insensitive: name == "--iglob",
                });
        }
        "--type" => request.file_types.push(value.to_owned()),
        "--type-not" => request.excluded_file_types.push(value.to_owned()),
        "--ignore-file" => request.ignore_files.push(value.into()),
        "--max-depth" => request.max_depth = Some(parse_usize(name, value)?),
        "--max-filesize" => {
            request.max_file_size_bytes =
                Some(parse_byte_size(value).map_err(|_| invalid(name, value))?);
        }
        "--before-context" => request.rg_options.before_context = parse_usize(name, value)?,
        "--after-context" => request.rg_options.after_context = parse_usize(name, value)?,
        "--context" => {
            let count = parse_usize(name, value)?;
            request.rg_options.before_context = count;
            request.rg_options.after_context = count;
        }
        "--max-count" => request.rg_options.max_count = Some(parse_usize(name, value)?),
        "--threads" => request.rg_options.threads = Some(parse_usize(name, value)?),
        "--regex-size-limit" | "--dfa-size-limit" => {
            let bytes = parse_byte_size(value)
                .ok()
                .and_then(|bytes| usize::try_from(bytes).ok())
                .ok_or_else(|| invalid(name, value))?;
            if name == "--regex-size-limit" {
                request.rg_options.regex_size_limit = Some(bytes);
            } else {
                request.rg_options.dfa_size_limit = Some(bytes);
            }
        }
        "--engine" if value == "default" => {}
        _ => {
            return Err(ManagedRgArgumentError::UnsupportedOption(format!(
                "{name}={value}"
            )));
        }
    }
    Ok(())
}

fn is_output_option(value: &str) -> bool {
    matches!(
        value.split_once('=').map_or(value, |(name, _)| name),
        "--count"
            | "--count-matches"
            | "--files"
            | "--files-with-matches"
            | "--files-without-match"
            | "--column"
            | "--byte-offset"
            | "--no-column"
            | "--colors"
            | "--context-separator"
            | "--field-context-separator"
            | "--field-match-separator"
            | "--json"
            | "--heading"
            | "--no-heading"
            | "--no-filename"
            | "--no-line-number"
            | "--only-matching"
            | "--passthru"
            | "--path-separator"
            | "--quiet"
            | "--pretty"
            | "--replace"
            | "--stats"
            | "--trim"
            | "--vimgrep"
            | "-c"
            | "-b"
            | "-I"
            | "-l"
            | "-N"
            | "-o"
            | "-p"
            | "-q"
            | "-r"
    )
}

fn take_value(
    args: &[String],
    index: &mut usize,
    option: &str,
) -> Result<String, ManagedRgArgumentError> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| ManagedRgArgumentError::MissingOptionValue {
            option: option.to_owned(),
        })
}

fn parse_usize(option: &str, value: &str) -> Result<usize, ManagedRgArgumentError> {
    value.parse().map_err(|_| invalid(option, value))
}

fn invalid(option: &str, value: &str) -> ManagedRgArgumentError {
    ManagedRgArgumentError::InvalidOptionValue {
        option: option.to_owned(),
        value: value.to_owned(),
    }
}

fn non_empty(option: &str, value: &str) -> Result<String, ManagedRgArgumentError> {
    if value.is_empty() {
        return Err(ManagedRgArgumentError::MissingOptionValue {
            option: option.to_owned(),
        });
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::parse_managed_rg_args;

    #[test]
    fn rejects_unsupported_features_during_parsing() {
        for flag in [
            "-P",
            "--pcre2",
            "-z",
            "--search-zip",
            "--encoding=utf-8",
            "--engine=auto",
            "--engine=pcre2",
            "--mmap",
            "--smart-case=true",
            "--json",
        ] {
            assert!(
                parse_managed_rg_args(&[flag.into(), "needle".into()]).is_err(),
                "{flag}"
            );
        }
    }

    #[test]
    fn validates_values_and_keeps_literal_equals_in_patterns() {
        for args in [
            vec!["--max-count"],
            vec!["--threads", "-1"],
            vec!["--glob="],
            vec!["--regex-size-limit=bogus"],
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(parse_managed_rg_args(&args).is_err(), "{args:?}");
        }
        for pattern in ["a=b", "--glob=x"] {
            let request =
                parse_managed_rg_args(&["--".into(), pattern.into()]).expect("literal pattern");
            assert_eq!(request.query.as_deref(), Some(pattern));
        }
        let request = parse_managed_rg_args(&["-e".into(), String::new(), "file".into()])
            .expect("empty regex");
        assert_eq!(request.queries, [""]);
    }
}
