use crate::domain::decode_text;

use super::{FileFormat, normalize_formats};

/// Infers a format for empty input; otherwise only filters existing candidates.
pub(super) fn refine(
    mut format_candidates: Vec<FileFormat>,
    sample_bytes: &[u8],
    complete: bool,
) -> Vec<FileFormat> {
    let detected_format = detect(sample_bytes, complete);
    if format_candidates.is_empty() {
        format_candidates.push(detected_format);
        return format_candidates;
    }
    match detected_format {
        FileFormat::Unknown => format_candidates.clear(),
        FileFormat::Text => {
            // Text can rule out a transport stream, but does not identify a language.
            format_candidates.retain(|format| *format != FileFormat::Mpeg);
        }
        _ => format_candidates.retain(|format| compatible_with(*format, detected_format)),
    }
    normalize_formats(&mut format_candidates);
    format_candidates
}

fn compatible_with(format: FileFormat, detected: FileFormat) -> bool {
    if format == detected {
        return true;
    }
    match detected {
        FileFormat::Zip => matches!(
            format,
            FileFormat::Word
                | FileFormat::Excel
                | FileFormat::PowerPoint
                | FileFormat::Keynote
                | FileFormat::Pages
                | FileFormat::Numbers
                | FileFormat::Epub
                | FileFormat::Jar
                | FileFormat::Odg
                | FileFormat::Odp
                | FileFormat::Ods
                | FileFormat::Odt
                | FileFormat::Xps
                | FileFormat::Visio
        ),
        FileFormat::Gzip => matches!(format, FileFormat::Tar | FileFormat::Svg),
        FileFormat::Ogg => format == FileFormat::Opus,
        // A Node shebang also appears in TypeScript source files.
        FileFormat::JavaScript => format == FileFormat::TypeScript,
        FileFormat::Xml => format == FileFormat::Svg,
        FileFormat::Svg => format == FileFormat::Xml,
        _ => false,
    }
}

/// Infers a format from a bounded prefix; `complete` means the reader reached EOF.
fn detect(bytes: &[u8], complete: bool) -> FileFormat {
    if let Some(format) = signature(bytes) {
        return format;
    }
    if let Some(text) = decode_text(bytes, complete).filter(|text| is_readable_text(text)) {
        return shebang(&text, complete)
            .or_else(|| markup(&text))
            .or_else(|| pem(&text))
            .unwrap_or(FileFormat::Text);
    }
    // Packet alignment is weaker evidence than a readable text sample.
    if is_transport_stream(bytes) {
        FileFormat::Mpeg
    } else {
        FileFormat::Unknown
    }
}

fn is_readable_text(text: &str) -> bool {
    !text.trim().is_empty()
        && !text
            .chars()
            .any(|ch| ch.is_control() && !ch.is_whitespace())
}

fn signature(bytes: &[u8]) -> Option<FileFormat> {
    // Prefix signatures, not container validation or identification of embedded formats.
    const SIGNATURES: &[(&[u8], FileFormat)] = &[
        (b"\x89PNG\r\n\x1a\n", FileFormat::Png),
        (b"\xff\xd8\xff", FileFormat::Jpeg),
        (b"GIF87a", FileFormat::Gif),
        (b"GIF89a", FileFormat::Gif),
        (b"II\x2a\x00", FileFormat::Tiff),
        (b"MM\x00\x2a", FileFormat::Tiff),
        (b"II\x2b\x00\x08\x00\x00\x00", FileFormat::Tiff),
        (b"MM\x00\x2b\x00\x08\x00\x00", FileFormat::Tiff),
        (
            b"8BPS\x00\x01\x00\x00\x00\x00\x00\x00",
            FileFormat::Photoshop,
        ),
        (
            b"8BPS\x00\x02\x00\x00\x00\x00\x00\x00",
            FileFormat::Photoshop,
        ),
        (b"%PDF-", FileFormat::Pdf),
        (b"PK\x03\x04", FileFormat::Zip),
        (b"PK\x05\x06", FileFormat::Zip),
        (b"\x1f\x8b\x08", FileFormat::Gzip),
        (b"Rar!\x1a\x07\x00", FileFormat::Rar),
        (b"Rar!\x1a\x07\x01\x00", FileFormat::Rar),
        (b"OggS\x00", FileFormat::Ogg),
        // FLAC begins with a 34-byte STREAMINFO block, optionally the last metadata block.
        (b"fLaC\x00\x00\x00\x22", FileFormat::Flac),
        (b"fLaC\x80\x00\x00\x22", FileFormat::Flac),
    ];
    if let Some((_, format)) = SIGNATURES
        .iter()
        .find(|(prefix, _)| bytes.starts_with(prefix))
    {
        return Some(*format);
    }
    if bytes.starts_with(b"BM")
        && bytes.len() >= 14
        && bytes.get(6..10) == Some(b"\x00\x00\x00\x00")
    {
        return Some(FileFormat::Bmp);
    }
    if bytes.starts_with(b"\x00\x00\x01\x00")
        && bytes.get(4..6).is_some_and(|count| count != [0, 0])
    {
        return Some(FileFormat::Ico);
    }
    if bytes.starts_with(b"RIFF") {
        if matches!(
            bytes.get(8..16),
            Some(b"WEBPVP8 " | b"WEBPVP8L" | b"WEBPVP8X")
        ) {
            return Some(FileFormat::Webp);
        }
        if bytes.get(8..12) == Some(b"WAVE") {
            return Some(FileFormat::Wav);
        }
        if bytes.get(8..12) == Some(b"AVI ") {
            return Some(FileFormat::Avi);
        }
    }
    if bytes.starts_with(b"FORM") && matches!(bytes.get(8..12), Some(b"AIFF" | b"AIFC")) {
        return Some(FileFormat::Aiff);
    }
    None
}

fn is_transport_stream(bytes: &[u8]) -> bool {
    // Four consecutive packet headers: TS, M2TS, or TS with error correction.
    [(188, 0), (192, 4), (204, 0)]
        .into_iter()
        .any(|(stride, offset)| {
            bytes.len() >= stride * 4
                && (0..4).all(|index| {
                    let header = offset + index * stride;
                    bytes[header] == 0x47
                        && bytes[header + 1] & 0x80 == 0
                        && bytes[header + 3] & 0x30 != 0
                })
        })
}

fn pem(text: &str) -> Option<FileFormat> {
    for line in text.split(['\r', '\n']).map(str::trim) {
        // Only explicit export metadata may precede the PEM marker.
        if line.is_empty()
            || line == "Bag Attributes"
            || [
                "Bag Attributes:",
                "Key Attributes:",
                "friendlyName:",
                "localKeyID:",
                "Microsoft CSP Name:",
                "subject=",
                "issuer=",
            ]
            .iter()
            .any(|prefix| line.starts_with(prefix))
        {
            continue;
        }
        let label = line.strip_prefix("-----BEGIN ")?.strip_suffix("-----")?;
        return matches!(
            label,
            "CERTIFICATE"
                | "X509 CERTIFICATE"
                | "X.509 CERTIFICATE"
                | "X509 CRL"
                | "TRUSTED CERTIFICATE"
                | "CERTIFICATE REQUEST"
                | "NEW CERTIFICATE REQUEST"
                | "PUBLIC KEY"
                | "RSA PUBLIC KEY"
                | "PRIVATE KEY"
                | "ENCRYPTED PRIVATE KEY"
                | "RSA PRIVATE KEY"
                | "DSA PRIVATE KEY"
                | "EC PRIVATE KEY"
                | "OPENSSH PRIVATE KEY"
                | "PKCS7"
                | "CMS"
                | "ATTRIBUTE CERTIFICATE"
                | "DH PARAMETERS"
                | "X9.42 DH PARAMETERS"
                | "EC PARAMETERS"
        )
        .then_some(FileFormat::Pem);
    }
    None
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

fn markup(text: &str) -> Option<FileFormat> {
    let mut start = text.trim_start();
    let xml = start
        .strip_prefix("<?xml")
        .is_some_and(|rest| rest.starts_with(char::is_whitespace));
    if xml {
        let Some((_, rest)) = start.split_once("?>") else {
            return Some(FileFormat::Xml);
        };
        start = rest.trim_start();
    }
    loop {
        let rest = if let Some(comment) = start.strip_prefix("<!--") {
            comment.split_once("-->").map(|(_, rest)| rest)
        } else if let Some(instruction) = start.strip_prefix("<?") {
            instruction.split_once("?>").map(|(_, rest)| rest)
        } else if markup_token(start, "<!DOCTYPE", false)
            && !markup_token(start, "<!doctype html", true)
        {
            after_doctype(start)
        } else {
            break;
        };
        let Some(rest) = rest else {
            break;
        };
        start = rest.trim_start();
    }
    if markup_token(start, "<svg", false) {
        return Some(FileFormat::Svg);
    }
    if xml {
        return Some(FileFormat::Xml);
    }
    if ["<!doctype html", "<html", "<head", "<body"]
        .iter()
        .any(|token| markup_token(start, token, true))
    {
        return Some(FileFormat::Html);
    }
    None
}

fn after_doctype(text: &str) -> Option<&str> {
    let declaration = text.strip_prefix("<!DOCTYPE")?;
    let bytes = declaration.as_bytes();
    let mut quote = None;
    let mut subset_depth = 0usize;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(delimiter) = quote {
            if byte == delimiter {
                quote = None;
            }
        } else if bytes[index..].starts_with(b"<!--") {
            index += 4 + declaration[index + 4..].find("-->")? + 3;
            continue;
        } else if bytes[index..].starts_with(b"<?") {
            index += 2 + declaration[index + 2..].find("?>")? + 2;
            continue;
        } else {
            match byte {
                b'\'' | b'"' => quote = Some(byte),
                b'[' => subset_depth += 1,
                b']' => subset_depth = subset_depth.checked_sub(1)?,
                b'>' if subset_depth == 0 => return Some(&declaration[index + 1..]),
                _ => {}
            }
        }
        index += 1;
    }
    None
}

fn markup_token(text: &str, token: &str, ignore_case: bool) -> bool {
    let Some(prefix) = text.get(..token.len()) else {
        return false;
    };
    let matches = if ignore_case {
        prefix.eq_ignore_ascii_case(token)
    } else {
        prefix == token
    };
    matches
        && text
            .as_bytes()
            .get(token.len())
            .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'>' | b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use FileFormat::*;

    #[test]
    fn refines_candidates() {
        let cases: &[(&[FileFormat], &[u8], &[FileFormat])] = &[
            (&[Word, Excel], b"PK\x03\x04", &[Excel, Word]),
            (&[Tar, Svg], b"\x1f\x8b\x08", &[Svg, Tar]),
            (&[Opus], b"OggS\x00", &[Opus]),
            (&[Mpeg, TypeScript], b"const value = 1;", &[TypeScript]),
            (&[Word], b"%PDF-1.7", &[]),
            (&[Json, TypeScript], b"", &[]),
            (&[Mpeg, TypeScript], b"\0\xff", &[]),
            (&[], b"\0\xff", &[Unknown]),
        ];
        for &(formats, bytes, expected) in cases {
            assert_eq!(
                refine(formats.to_vec(), bytes, true),
                expected,
                "{formats:?}"
            );
        }
    }

    #[test]
    fn recognizes_file_signatures() {
        for (bytes, expected) in [
            (b"\x89PNG\r\n\x1a\n".as_slice(), Png),
            (b"\xff\xd8\xff\xe0", Jpeg),
            (b"GIF89a", Gif),
            (b"BM\x3a\0\0\0\0\0\0\0\x36\0\0\0", Bmp),
            (b"II\x2a\0\x08\0\0\0", Tiff),
            (b"MM\0\x2a\0\0\0\x08", Tiff),
            (b"II\x2b\0\x08\0\0\0\x10\0\0\0\0\0\0\0", Tiff),
            (b"MM\0\x2b\0\x08\0\0\0\0\0\0\0\0\0\x10", Tiff),
            (b"\0\0\x01\0\x01\0", Ico),
            (b"8BPS\0\x01\0\0\0\0\0\0", Photoshop),
            (b"8BPS\0\x02\0\0\0\0\0\0", Photoshop),
            (b"%PDF-1.7", Pdf),
            (b"PK\x03\x04word/document.xml", Zip),
            (b"PK\x05\x06", Zip),
            (b"\x1f\x8b\x08", Gzip),
            (b"Rar!\x1a\x07\0", Rar),
            (b"RIFF\x16\0\0\0WEBPVP8 ", Webp),
            (b"RIFF\x16\0\0\0WEBPVP8L", Webp),
            (b"RIFF\x2a\0\0\0WEBPVP8X", Webp),
            (b"RIFF\x24\0\0\0WAVE", Wav),
            (b"RIFF\x78\0\0\0AVI ", Avi),
            (b"FORM\0\0\0\x2eAIFF", Aiff),
            (b"FORM\0\0\0\x42AIFC", Aiff),
            (b"OggS\0", Ogg),
            (b"fLaC\0\0\0\x22", Flac),
            (b"fLaC\x80\0\0\x22", Flac),
        ] {
            assert_eq!(detect(bytes, true), expected, "{bytes:?}");
        }
        for bytes in [
            b"\x89PNG".as_slice(),
            b"RIFF\x16\0\0\0WEBPVPZZ",
            b"BM ordinary text",
            b"II\x2b\0\x04\0\0\0",
            b"\0\0\x01\0\0\0",
            b"8BPS\0\x03\0\0\0\0\0\0",
            b"fLaC\x81\0\0\x22",
            b"FORM\0\0\0\x2eILBM",
        ] {
            assert_eq!(signature(bytes), None, "{bytes:?}");
        }
    }

    #[test]
    fn decodes_text_conservatively() {
        let text = "hello 中文 🦀";
        for bytes in [
            text.as_bytes().to_vec(),
            utf16(text, true),
            utf16(text, false),
            utf32(text, true),
            utf32(text, false),
        ] {
            assert_eq!(detect(&bytes, true), Text, "{bytes:?}");
            for missing in 1..=3 {
                let truncated = &bytes[..bytes.len() - missing];
                assert_eq!(detect(truncated, false), Text, "{truncated:?}");
                assert_eq!(detect(truncated, true), Unknown, "{truncated:?}");
            }
        }
        for bytes in [b"\t hello\r\n ".as_slice(), b"\xef\xbb\xbfhello"] {
            assert_eq!(detect(bytes, true), Text);
        }
        for bytes in [
            b"".as_slice(),
            " \t\r\n\u{2003}\u{2028}".as_bytes(),
            b"hello\0world",
            b"hello\x01world",
            b"\x1b[31mred\x1b[0m",
            b"hello\xff",
            b"\xe4\xb8",
            b"\xef\xbb\xbf",
            b"\xff\xfe",
            b"\xff\xfeA\0\0\xd8B\0",
            b"\xff\xfe\0\0",
            b"\xff\xfe\0\0\0\0\x11\0",
        ] {
            for complete in [true, false] {
                assert_eq!(detect(bytes, complete), Unknown, "{bytes:?}, {complete}");
            }
        }
    }

    #[test]
    fn recognizes_text_formats() {
        for (text, expected) in [
            ("#!/usr/bin/python3.12\nprint(1)", Python),
            ("#!/usr/bin/env -S python3 -u\nprint(1)", Python),
            ("#!/usr/bin/env -i MODE=test node\r\n", JavaScript),
            ("#!/usr/bin/env -- python3\n", Python),
            ("<!-- intro -->\n<HTML lang='en'>", Html),
            ("<?xml version='1.0'?><root/>", Xml),
            ("<?xml version='1.0'?>\n<!-- intro -->\n<svg/>", Svg),
            ("-----BEGIN X509 CRL-----\nMIIB", Pem),
            ("-----BEGIN PRIVATE KEY-----\rMIIB", Pem),
            ("-----BEGIN OPENSSH PRIVATE KEY-----\nb3Bl", Pem),
            ("Bag Attributes\n  -----BEGIN CERTIFICATE-----  \nMIIB", Pem),
        ] {
            assert_eq!(detect(text.as_bytes(), false), expected, "{text}");
        }
        for text in [
            "#!/usr/bin/python3-config\n",
            "#!/usr/bin/env -u NAME python3\n",
            "#!/usr/bin/env -- -S python3\n",
            "notes\n#!/bin/bash\n",
            "<htmlish>",
            "<svgish>",
            "<?xml-stylesheet href='style.xsl'?>",
            "-----BEGIN NOTES-----\nhello",
            "-----BEGIN PRIVATE KEY----\nMIIB",
            "Notes about -----BEGIN PRIVATE KEY-----",
        ] {
            assert_eq!(detect(text.as_bytes(), true), Text, "{text}");
        }
        assert_eq!(detect(b"#!/usr/bin/python3", true), Python);
        assert_eq!(detect(b"#!/usr/bin/python3", false), Text);
        assert_eq!(detect(&utf16("<svg/>", true), true), Svg);
        assert_eq!(detect(b"<html>\n\0", true), Unknown);

        // Embedded PEM examples must not override an explicit script or document header.
        let pem = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----";
        for (text, expected) in [
            (format!("#!/bin/sh\ncat <<'EOF'\n{pem}\nEOF\n"), Shell),
            (
                format!("<!DOCTYPE html>\n<html><pre>\n{pem}\n</pre></html>"),
                Html,
            ),
        ] {
            assert_eq!(detect(text.as_bytes(), true), expected);
        }
    }

    fn utf16(text: &str, little_endian: bool) -> Vec<u8> {
        let mut bytes = if little_endian {
            vec![0xff, 0xfe]
        } else {
            vec![0xfe, 0xff]
        };
        for unit in text.encode_utf16() {
            bytes.extend(if little_endian {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            });
        }
        bytes
    }

    fn utf32(text: &str, little_endian: bool) -> Vec<u8> {
        let mut bytes = if little_endian {
            vec![0xff, 0xfe, 0, 0]
        } else {
            vec![0, 0, 0xfe, 0xff]
        };
        for ch in text.chars() {
            let unit = u32::from(ch);
            bytes.extend(if little_endian {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            });
        }
        bytes
    }
}
