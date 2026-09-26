use std::borrow::Cow;

use encoding_rs::Encoding;

use crate::domain::FileFormat;

/// How much of a file is searched for an in-band encoding declaration.
const DECLARATION_WINDOW: usize = 1024;

/// Decodes a known text format for indexing. Format sniffing remains strict.
pub(crate) fn decode_index_text<'a>(
    formats: &[FileFormat],
    bytes: &'a [u8],
) -> Option<Cow<'a, str>> {
    if has_bom(bytes) {
        return decode_text(bytes, true);
    }
    let declared = declared_encoding(formats, bytes);
    // Python honours its cookie even when the bytes happen to be valid UTF-8.
    if formats.contains(&FileFormat::Python) && declared == Some(Declared::Latin1) {
        return Some(Cow::Owned(decode_latin_one(bytes)));
    }
    if let Some(text) = decode_text(bytes, true) {
        return Some(text);
    }
    if looks_binary(bytes) {
        return None;
    }
    Some(match declared {
        Some(Declared::Latin1) => Cow::Owned(decode_latin_one(bytes)),
        Some(Declared::Other(encoding)) => encoding.decode_without_bom_handling(bytes).0,
        None => String::from_utf8_lossy(bytes),
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Declared {
    /// True ISO-8859-1, where every byte maps to the code point of the same value.
    Latin1,
    Other(&'static Encoding),
}

fn declared_encoding(formats: &[FileFormat], bytes: &[u8]) -> Option<Declared> {
    let head = &bytes[..bytes.len().min(DECLARATION_WINDOW)];
    let has = |format| formats.contains(&format);
    // HTML and CSS follow the Encoding Standard, which reads latin1 as windows-1252.
    if has(FileFormat::Html) {
        return html_meta_charset(head).and_then(web_label);
    }
    if has(FileFormat::Css) || has(FileFormat::Less) || has(FileFormat::Sass) {
        return css_charset(head).and_then(web_label);
    }
    let label = if has(FileFormat::Xml) || has(FileFormat::Svg) {
        xml_declared_encoding(head)
    } else if has(FileFormat::Python) {
        coding_comment(head, starts_with_hash, |first| {
            let first = first.trim_ascii_start();
            first.is_empty() || first[0] == b'#'
        })
    } else if has(FileFormat::Ruby) {
        coding_comment(head, starts_with_hash, is_shebang)
    } else {
        // Emacs reads a coding tag from either of the first two lines.
        coding_comment(head, |line| find(line, b"-*-").is_some(), |_| true)
            .map(strip_emacs_eol_suffix)
    };
    label.and_then(legacy_label)
}

/// Resolves a label the way browsers do.
fn web_label(label: &[u8]) -> Option<Declared> {
    Encoding::for_label(label)
        .filter(|encoding| encoding.is_ascii_compatible())
        .map(Declared::Other)
}

/// Resolves a label from a format whose latin1 means true ISO-8859-1.
fn legacy_label(label: &[u8]) -> Option<Declared> {
    if is_latin_one_alias(label) {
        return Some(Declared::Latin1);
    }
    // Python and Ruby spell names like euc_jp with underscores.
    let dashed: Vec<u8> = label
        .iter()
        .map(|byte| if *byte == b'_' { b'-' } else { *byte })
        .collect();
    web_label(label).or_else(|| web_label(&dashed))
}

fn decode_latin_one(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| char::from(*byte)).collect()
}

fn has_bom(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\xef\xbb\xbf")
        || bytes.starts_with(b"\xff\xfe")
        || bytes.starts_with(b"\xfe\xff")
        || bytes.starts_with(b"\x00\x00\xfe\xff")
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .any(|byte| matches!(*byte, 0 | 1..=8 | 11 | 14..=31))
}

fn starts_with_hash(line: &[u8]) -> bool {
    line.trim_ascii_start().starts_with(b"#")
}

fn is_shebang(line: &[u8]) -> bool {
    line.starts_with(b"#!")
}

/// Reads a `coding[:=] label` comment from line one, or from line two when
/// `line_two_allowed` accepts line one.
fn coding_comment(
    head: &[u8],
    is_declaration_line: fn(&[u8]) -> bool,
    line_two_allowed: fn(&[u8]) -> bool,
) -> Option<&[u8]> {
    let mut lines = head.split(|byte| *byte == b'\n');
    let first = lines.next().unwrap_or_default();
    if is_declaration_line(first)
        && let Some(label) = coding_label(first)
    {
        return Some(label);
    }
    if !line_two_allowed(first) {
        return None;
    }
    lines
        .next()
        .filter(|line| is_declaration_line(line))
        .and_then(coding_label)
}

fn coding_label(line: &[u8]) -> Option<&[u8]> {
    let mut rest = line;
    while let Some(start) = find(rest, b"coding") {
        rest = &rest[start + 6..];
        let Some(after) = rest.strip_prefix(b":").or_else(|| rest.strip_prefix(b"=")) else {
            continue;
        };
        let label = take_label(after.trim_ascii_start());
        if !label.is_empty() {
            return Some(label);
        }
    }
    None
}

/// Emacs appends the end-of-line convention, as in `latin-1-unix`.
fn strip_emacs_eol_suffix(label: &[u8]) -> &[u8] {
    [b"-unix".as_slice(), b"-dos", b"-mac"]
        .iter()
        .find_map(|suffix| {
            label
                .len()
                .checked_sub(suffix.len())
                .filter(|split| label[*split..].eq_ignore_ascii_case(suffix))
                .map(|split| &label[..split])
        })
        .unwrap_or(label)
}

/// Reads `encoding="..."` from an XML declaration, which must open the file.
fn xml_declared_encoding(head: &[u8]) -> Option<&[u8]> {
    let declaration = head.strip_prefix(b"<?xml")?;
    let declaration = &declaration[..find(declaration, b"?>")?];
    let rest = &declaration[find(declaration, b"encoding")? + 8..];
    let rest = rest
        .trim_ascii_start()
        .strip_prefix(b"=")?
        .trim_ascii_start();
    let quote = *rest
        .first()
        .filter(|quote| matches!(**quote, b'"' | b'\''))?;
    let value = &rest[1..];
    Some(&value[..value.iter().position(|byte| *byte == quote)?])
}

/// Reads the charset from the first `<meta>` tag that declares one, either as a
/// `charset` attribute or as `http-equiv="Content-Type"` content. Comments are
/// skipped; this is a subset of the HTML prescan, not the full algorithm.
fn html_meta_charset(head: &[u8]) -> Option<&[u8]> {
    let mut rest = head;
    while let Some(start) = rest.iter().position(|byte| *byte == b'<') {
        rest = &rest[start..];
        if let Some(comment) = rest.strip_prefix(b"<!--") {
            rest = find(comment, b"-->").map_or(&[][..], |end| &comment[end + 3..]);
            continue;
        }
        let is_meta = rest.len() > 5
            && rest[1..5].eq_ignore_ascii_case(b"meta")
            && (rest[5].is_ascii_whitespace() || rest[5] == b'/');
        if !is_meta {
            rest = &rest[1..];
            continue;
        }
        let (attributes, after) = meta_attributes(&rest[5..]);
        rest = after;
        if let Some(label) = meta_charset(&attributes) {
            return Some(label);
        }
    }
    None
}

fn meta_charset<'a>(attributes: &[Attribute<'a>]) -> Option<&'a [u8]> {
    let value = |name: &[u8]| {
        attributes
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| *value)
    };
    if let Some(charset) = value(b"charset") {
        return Some(take_label(charset.trim_ascii())).filter(|label| !label.is_empty());
    }
    if !value(b"http-equiv")?
        .trim_ascii()
        .eq_ignore_ascii_case(b"content-type")
    {
        return None;
    }
    let content = value(b"content")?;
    let after = &content[find_ignore_case(content, b"charset")? + 7..];
    let after = after
        .trim_ascii_start()
        .strip_prefix(b"=")?
        .trim_ascii_start();
    let after = after
        .strip_prefix(b"\"")
        .or_else(|| after.strip_prefix(b"'"))
        .unwrap_or(after);
    Some(take_label(after)).filter(|label| !label.is_empty())
}

/// A tag attribute's name and unquoted value.
type Attribute<'a> = (&'a [u8], &'a [u8]);

/// Parses tag attributes up to the closing `>`, honouring quoted values.
fn meta_attributes(mut rest: &[u8]) -> (Vec<Attribute<'_>>, &[u8]) {
    let mut attributes = Vec::new();
    loop {
        rest = rest.trim_ascii_start();
        match rest.first() {
            None => return (attributes, rest),
            Some(b'>') => return (attributes, &rest[1..]),
            Some(b'/') => {
                rest = &rest[1..];
                continue;
            }
            Some(_) => {}
        }
        let end = rest
            .iter()
            .position(|byte| byte.is_ascii_whitespace() || matches!(*byte, b'=' | b'>' | b'/'))
            .unwrap_or(rest.len());
        let name = &rest[..end];
        rest = rest[end..].trim_ascii_start();
        let Some(after) = rest.strip_prefix(b"=") else {
            attributes.push((name, &[][..]));
            continue;
        };
        rest = after.trim_ascii_start();
        let value;
        if let Some(&quote) = rest.first().filter(|byte| matches!(**byte, b'"' | b'\'')) {
            let body = &rest[1..];
            let end = body
                .iter()
                .position(|byte| *byte == quote)
                .unwrap_or(body.len());
            value = &body[..end];
            rest = body.get(end + 1..).unwrap_or_default();
        } else {
            let end = rest
                .iter()
                .position(|byte| byte.is_ascii_whitespace() || *byte == b'>')
                .unwrap_or(rest.len());
            value = &rest[..end];
            rest = &rest[end..];
        }
        attributes.push((name, value));
    }
}

/// Reads the `@charset "...";` rule, which CSS only honours byte-for-byte at the start.
fn css_charset(head: &[u8]) -> Option<&[u8]> {
    let value = head.strip_prefix(b"@charset \"")?;
    let end = value.iter().position(|byte| *byte == b'"')?;
    value[end + 1..].starts_with(b";").then(|| &value[..end])
}

fn take_label(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .position(|byte| {
            !byte.is_ascii_alphanumeric() && !matches!(*byte, b'-' | b'_' | b'.' | b':')
        })
        .unwrap_or(bytes.len());
    &bytes[..end]
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn find_ignore_case(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

/// Matches the ISO-8859-1 names used by Python, Ruby, Emacs, and IANA (for XML).
fn is_latin_one_alias(label: &[u8]) -> bool {
    let normalized: Vec<u8> = label
        .iter()
        .map(|byte| match byte.to_ascii_lowercase() {
            b'-' | b':' => b'_',
            byte => byte,
        })
        .collect();
    [
        b"8859".as_slice(),
        b"cp819",
        b"csisolatin1",
        b"ibm819",
        b"iso8859",
        b"iso8859_1",
        b"iso_8859_1",
        b"iso_8859_1_1987",
        b"iso_ir_100",
        b"iso_latin_1",
        b"l1",
        b"latin",
        b"latin1",
        b"latin_1",
    ]
    .contains(&normalized.as_slice())
}

/// Decodes UTF-8, or UTF-16/32 with a BOM, without replacing invalid input.
pub(crate) fn decode_text(bytes: &[u8], complete: bool) -> Option<Cow<'_, str>> {
    // UTF-32 LE must precede UTF-16 LE because their BOMs share the first two bytes.
    if let Some(body) = bytes.strip_prefix(b"\xff\xfe\x00\x00") {
        return decode_utf32(body, true, complete).map(Cow::Owned);
    }
    if let Some(body) = bytes.strip_prefix(b"\x00\x00\xfe\xff") {
        return decode_utf32(body, false, complete).map(Cow::Owned);
    }
    if let Some(body) = bytes.strip_prefix(b"\xff\xfe") {
        return decode_utf16(body, true, complete).map(Cow::Owned);
    }
    if let Some(body) = bytes.strip_prefix(b"\xfe\xff") {
        return decode_utf16(body, false, complete).map(Cow::Owned);
    }
    let body = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let text = match std::str::from_utf8(body) {
        Ok(text) => text,
        Err(error) if !complete && error.error_len().is_none() => {
            std::str::from_utf8(&body[..error.valid_up_to()]).ok()?
        }
        Err(_) => return None,
    };
    Some(Cow::Borrowed(text))
}

fn decode_utf16(bytes: &[u8], little_endian: bool, complete: bool) -> Option<String> {
    let (mut units, remainder) = bytes.as_chunks::<2>();
    if complete && !remainder.is_empty() {
        return None;
    }
    let unit = |pair: &[u8; 2]| {
        if little_endian {
            u16::from_le_bytes(*pair)
        } else {
            u16::from_be_bytes(*pair)
        }
    };
    // A sample may end halfway through a surrogate pair as well as a code unit.
    if !complete
        && units
            .last()
            .is_some_and(|pair| (0xd800..=0xdbff).contains(&unit(pair)))
    {
        units = &units[..units.len() - 1];
    }
    char::decode_utf16(units.iter().map(unit))
        .collect::<Result<String, _>>()
        .ok()
}

fn decode_utf32(bytes: &[u8], little_endian: bool, complete: bool) -> Option<String> {
    let (units, remainder) = bytes.as_chunks::<4>();
    if complete && !remainder.is_empty() {
        return None;
    }
    units
        .iter()
        .map(|unit| {
            char::from_u32(if little_endian {
                u32::from_le_bytes(*unit)
            } else {
                u32::from_be_bytes(*unit)
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::decode_index_text;
    use crate::domain::FileFormat;

    #[test]
    fn python_latin_one_cookie_preserves_legacy_characters() {
        let bytes = b"#!/usr/bin/env python\n# coding: latin_1\nTOTO = 'Caf\xe9'\n";
        let text = decode_index_text(&[FileFormat::Python], bytes).expect("Latin-1 Python");
        assert!(text.contains("Café"));
        assert!(!text.contains('\u{fffd}'));
    }

    #[test]
    fn first_python_cookie_takes_precedence_over_second() {
        let utf_eight = "# coding: utf-8\n# coding: latin1\nname = 'Café'\n";
        let text = decode_index_text(&[FileFormat::Python], utf_eight.as_bytes())
            .expect("UTF-8 declaration on first line");
        assert_eq!(text, utf_eight);

        let latin_one = b"# coding: latin1\n# coding: utf-8\nname = 'Caf\xe9'\n";
        let text = decode_index_text(&[FileFormat::Python], latin_one)
            .expect("Latin-1 declaration on first line");
        assert!(text.contains("Café"));
        assert!(!text.contains('\u{fffd}'));
    }

    #[test]
    fn blank_first_python_line_allows_second_line_cookie() {
        for first_line in [b"\n".as_slice(), b" \t\n", b" \t\x0c\r\n"] {
            let mut bytes = first_line.to_vec();
            bytes.extend_from_slice(b"# coding: latin_1\nname = 'Caf\xe9'\n");
            let text = decode_index_text(&[FileFormat::Python], &bytes)
                .expect("Latin-1 declaration on second line");
            assert!(text.contains("Café"));
            assert!(!text.contains('\u{fffd}'));
        }
    }

    #[test]
    fn python_code_on_first_line_ignores_second_line_cookie() {
        let bytes = b"value = 1\n# coding: latin1\nname = 'Caf\xe9'\n";
        let text = decode_index_text(&[FileFormat::Python], bytes)
            .expect("invalid UTF-8 is replaced without a valid cookie");
        assert!(text.contains("Caf\u{fffd}"));
    }

    #[test]
    fn invalid_utf_eight_bytes_are_replaced_without_guessing_latin_one() {
        let bytes = b"/* \xe9viter \xe9crasement */\n.test { margin: 1rem; }\n";
        let text = decode_index_text(&[FileFormat::Css], bytes).expect("legacy CSS");
        assert_eq!(text.matches('\u{fffd}').count(), 2);
        assert!(!text.contains('é'));
        assert!(text.contains(".test { margin: 1rem; }"));
        let dense = b"body { note: \xe9\xe9\xe9\xe9\xe9\xe9\xe9\xe9; }";
        let text = decode_index_text(&[FileFormat::Css], dense).expect("replace invalid bytes");
        assert_eq!(text.matches('\u{fffd}').count(), 8);
    }

    #[test]
    fn malformed_bom_and_binary_content_remain_errors() {
        assert!(decode_index_text(&[FileFormat::Text], &[0xff, 0xfe, 0xff]).is_none());
        let mut transport = vec![0xff; 188 * 3];
        for packet in transport.as_chunks_mut::<188>().0 {
            packet[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
        }
        assert!(decode_index_text(&[FileFormat::TypeScript], &transport).is_none());
    }

    #[test]
    fn xml_declaration_keeps_true_latin_one() {
        // 0x80 is a C1 control in ISO-8859-1 but the euro sign in windows-1252.
        let bytes = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>\n<a>Caf\xe9 \x80</a>\n";
        let text = decode_index_text(&[FileFormat::Xml], bytes).expect("Latin-1 XML");
        assert!(text.contains("Caf\u{e9} \u{80}"));

        let svg =
            b"<?xml version='1.0' encoding='Shift_JIS'?><svg><text>\x93\xfa\x96\x7b</text></svg>";
        let text = decode_index_text(&[FileFormat::Svg], svg).expect("Shift_JIS SVG");
        assert!(text.contains("日本"));
    }

    #[test]
    fn html_meta_charset_follows_the_encoding_standard() {
        let bytes = b"<html><head><meta charset=\"iso-8859-1\"></head><p>Caf\xe9 \x80</p>";
        let text = decode_index_text(&[FileFormat::Html], bytes).expect("windows-1252 HTML");
        assert!(text.contains("Café €"));

        let bytes = b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=gbk\">\xd6\xd0\xce\xc4";
        let text = decode_index_text(&[FileFormat::Html], bytes).expect("GBK HTML");
        assert!(text.contains("中文"));
    }

    #[test]
    fn css_charset_rule_must_open_the_file() {
        let bytes = b"@charset \"windows-1252\";\n/* \xe9viter */\n";
        let text = decode_index_text(&[FileFormat::Css], bytes).expect("windows-1252 CSS");
        assert!(text.contains("éviter"));

        let late = b"\n@charset \"windows-1252\";\n/* \xe9viter */\n";
        let text = decode_index_text(&[FileFormat::Css], late).expect("ignored @charset");
        assert!(text.contains("\u{fffd}viter"));
    }

    #[test]
    fn ruby_and_emacs_coding_comments_are_honoured() {
        let ruby = b"#!/usr/bin/env ruby\n# -*- encoding: euc_jp -*-\nputs '\xc6\xfc\xcb\xdc'\n";
        let text = decode_index_text(&[FileFormat::Ruby], ruby).expect("EUC-JP Ruby");
        assert!(text.contains("日本"));

        let c = b"/* -*- mode: c; coding: latin-1-unix -*- */\nchar *s = \"Caf\xe9 \x80\";\n";
        let text = decode_index_text(&[FileFormat::C], c).expect("Latin-1 C");
        assert!(text.contains("Caf\u{e9} \u{80}"));
    }

    #[test]
    fn python_cookie_decodes_other_declared_encodings() {
        let bytes = b"# -*- coding: cp1252 -*-\nname = 'Caf\xe9 \x80'\n";
        let text = decode_index_text(&[FileFormat::Python], bytes).expect("cp1252 Python");
        assert!(text.contains("Café €"));
    }

    #[test]
    fn declarations_do_not_override_valid_utf_eight() {
        let bytes = "<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><a>Café</a>";
        let text = decode_index_text(&[FileFormat::Xml], bytes.as_bytes()).expect("UTF-8 XML");
        assert_eq!(text, bytes);
    }

    #[test]
    fn unknown_or_ascii_incompatible_declarations_fall_back_to_replacement() {
        for bytes in [
            b"<?xml version=\"1.0\" encoding=\"no-such-charset\"?><a>Caf\xe9</a>".as_slice(),
            b"<?xml version=\"1.0\" encoding=\"UTF-16\"?><a>Caf\xe9</a>",
        ] {
            let text = decode_index_text(&[FileFormat::Xml], bytes).expect("replacement");
            assert!(text.contains("Caf\u{fffd}"));
        }
    }

    #[test]
    fn html_ignores_charset_text_outside_declarations() {
        for bytes in [
            b"<meta name=\"description\" content=\"charset=windows-1252\"><p>Caf\xe9</p>"
                .as_slice(),
            b"<!-- <meta charset=\"windows-1252\"> --><p>Caf\xe9</p>",
            b"<metadata charset=\"windows-1252\"><p>Caf\xe9</p>",
        ] {
            let text = decode_index_text(&[FileFormat::Html], bytes).expect("replacement");
            assert!(text.contains("Caf\u{fffd}"));
        }

        let bytes = b"<!-- note --><meta content='x>y' charset=windows-1252><p>Caf\xe9</p>";
        let text = decode_index_text(&[FileFormat::Html], bytes).expect("declared after comment");
        assert!(text.contains("Café"));
    }

    #[test]
    fn python_latin_one_aliases_keep_true_latin_one() {
        for label in [
            "L1",
            "latin",
            "iso8859",
            "cp819",
            "ISO_8859-1:1987",
            "iso-ir-100",
        ] {
            let bytes = [
                format!("# coding: {label}\nname = '").as_bytes(),
                b"\x80'\n",
            ]
            .concat();
            let text = decode_index_text(&[FileFormat::Python], &bytes).expect(label);
            assert!(text.contains('\u{80}'), "{label}");
        }
    }

    #[test]
    fn emacs_coding_tag_on_line_two_without_shebang() {
        let c = b"// header\n/* -*- coding: latin-1 -*- */\nchar *s = \"Caf\xe9 \x80\";\n";
        let text = decode_index_text(&[FileFormat::C], c).expect("Latin-1 C");
        assert!(text.contains("Caf\u{e9} \u{80}"));
    }
}
