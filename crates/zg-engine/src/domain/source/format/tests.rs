use std::{
    collections::{HashMap, HashSet},
    fs,
};

use tempfile::tempdir;

use super::*;
use FileFormat::*;

fn name_formats(file_name: &str) -> Vec<FileFormat> {
    // Empty files provide no content evidence beyond the name-based candidates.
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join(file_name);
    fs::write(&path, []).expect("write empty sample");
    let mut formats = FileFormat::from_path(&path).expect("name hints");
    formats.retain(|format| *format != Unknown);
    formats
}

#[test]
fn formats_have_query_names_and_categories() {
    use FileCategory as Category;

    let cases: &[(FileFormat, &str, &[FileCategory])] = &[
        (Unknown, "unknown", &[Category::Unknown]),
        (Rust, "rust", &[Category::Code]),
        (Text, "text", &[Category::Document]),
        (Word, "word", &[Category::Document]),
        (Excel, "excel", &[Category::Document]),
        (PowerPoint, "powerpoint", &[Category::Document]),
        (Pdf, "pdf", &[Category::Document]),
        (Json, "json", &[Category::Data]),
        (Xml, "xml", &[Category::Data]),
        (Git, "git", &[Category::Data]),
        (Html, "html", &[Category::Code, Category::Document]),
        (Latex, "latex", &[Category::Code, Category::Document]),
        (Odg, "odg", &[Category::Document, Category::Image]),
        (Svg, "svg", &[Category::Image]),
        (Mp3, "mp3", &[Category::Audio]),
        (Mpeg, "mpeg", &[Category::Video]),
        (Ogg, "ogg", &[Category::Audio, Category::Video]),
        (Tar, "tar", &[Category::Archive]),
        (Jar, "jar", &[Category::Archive, Category::Binary]),
        (Binary, "binary", &[Category::Binary]),
        (Arrow, "arrow", &[Category::Binary]),
        (Sqlite, "sqlite", &[Category::Binary]),
        (Pem, "pem", &[Category::Binary]),
        (ObjectiveC, "objective-c", &[Category::Code]),
        (JpegXl, "jpeg-xl", &[Category::Image]),
    ];
    for &(format, name, categories) in cases {
        assert_eq!(format.as_str(), name, "{format:?}");
        assert_eq!(format.categories(), categories, "{format:?}");
    }
}

#[test]
fn catalog_definitions_round_trip() {
    let mut identifiers = HashSet::new();
    let mut names = HashSet::new();
    for &format in catalog::FORMATS {
        let identifier = format as u16;
        assert!(identifiers.insert(identifier), "duplicate ID: {identifier}");
        assert_eq!(FileFormat::from_id(identifier), Some(format));

        let name = format.as_str();
        assert!(!name.is_empty());
        assert_eq!(name, name.trim());
        assert_eq!(name, name.to_ascii_lowercase());
        assert!(names.insert(name), "duplicate name: {name}");
    }
    assert_eq!(Unknown as u16, 0);
    assert_eq!(FileFormat::from_id(0), Some(Unknown));
    assert_eq!(FileFormat::from_id(u16::MAX), None);

    let mut extensions = HashMap::new();
    for &(extension, format) in catalog::EXTENSIONS {
        let candidates = extensions.entry(extension).or_insert_with(Vec::new);
        assert!(
            !candidates.contains(&format),
            "duplicate registration: {extension}, {format:?}"
        );
        candidates.push(format);
    }
    for (extension, mut candidates) in extensions {
        candidates.sort_unstable_by_key(|format| *format as u16);
        let name = format!("sample.{}", extension.to_ascii_uppercase());
        assert_eq!(name_formats(&name), candidates, "{name}");
    }

    let mut file_names = HashMap::new();
    for &(name, format) in catalog::FILE_NAMES {
        let candidates = file_names.entry(name).or_insert_with(Vec::new);
        assert!(
            !candidates.contains(&format),
            "duplicate registration: {name}, {format:?}"
        );
        candidates.push(format);
    }
    for (name, candidates) in file_names {
        assert_eq!(catalog::lookup_name(name), candidates, "{name}");
        let formats = name_formats(name);
        assert!(
            candidates.iter().all(|format| formats.contains(format)),
            "{name}: {formats:?}"
        );
    }
}

#[test]
fn file_names_resolve_registered_formats() {
    let cases: &[(&str, &[FileFormat])] = &[
        ("Dockerfile", &[Dockerfile]),
        ("Dockerfile.dev", &[Dockerfile]),
        ("Containerfile.production", &[Dockerfile]),
        ("Makefile", &[Makefile]),
        ("CMakeLists.txt", &[Cmake]),
        ("CMakeCache.txt", &[Cmake]),
        ("Cargo.lock", &[Toml]),
        (".gitignore", &[Git]),
        (".gitattributes", &[Git]),
        (".gitconfig", &[Git]),
        (".gitmodules", &[Git]),
        (".mailmap", &[Git]),
        (".env", &[Dotenv]),
        (".env.local", &[Dotenv]),
        (".bashrc", &[Bash]),
        ("tsconfig.json", &[Json, TypeScript]),
        ("tsconfig.build.json", &[Json, TypeScript]),
        ("jsconfig.json", &[JavaScript, Json]),
        ("CMakePresets.json", &[Cmake, Json]),
        ("Dockerfile.rs", &[Rust]),
        (".env.json", &[Json]),
        ("tsconfig.custom.json", &[Json]),
        ("TSCONFIG.json", &[Json]),
    ];
    for &(name, expected) in cases {
        assert_eq!(name_formats(name), expected, "{name}");
    }
    for name in [
        "dockerfile",
        "Dockerfile.custom",
        "Containerfile.custom",
        ".env.custom",
        "dockerfile.dev",
        ".ENV.local",
    ] {
        assert!(name_formats(name).is_empty(), "{name}");
    }

    let mut formats = vec![TypeScript, Json, Unknown, Json, Text];
    normalize_formats(&mut formats);
    assert_eq!(formats, [Json, TypeScript]);
}

#[test]
fn extensions_match_longest_registered_suffix() {
    let aliases: &[(&[&str], FileFormat)] = &[
        (&["jpg", "JPEG", "jfif"], Jpeg),
        (&["doc", "DOCX", "docm", "dotx"], Word),
        (&["xls", "XLSX", "xlsb"], Excel),
        (&["ppt", "pptx", "potm"], PowerPoint),
        (&["json", "JSONC", "json5"], Json),
        (&["js", "jsx", "mjs", "cjs"], JavaScript),
        (&["md", "markdown", "mdx"], Markdown),
        (&["odg", "otg", "fodg"], Odg),
        (&["ogg", "oga", "ogv"], Ogg),
        (&["woff", "woff2"], Woff),
    ];
    for &(extensions, expected) in aliases {
        for extension in extensions {
            let name = format!("sample.{extension}");
            assert_eq!(name_formats(&name), [expected], "{name}");
        }
    }
    let cases: &[(&str, &[FileFormat])] = &[
        ("scan.TiF", &[Tiff]),
        ("report.最终.PDF", &[Pdf]),
        (".config.JSON", &[Json]),
        ("backup.2026.TAR.GZ", &[Tar]),
        ("events.json.gz", &[Gzip]),
        ("data.notar.gz", &[Gzip]),
        (".tar.gz", &[Gzip]),
        ("module.d.ts", &[TypeScript]),
        ("module.D.MTS", &[TypeScript]),
        ("header.H", &[C, Cpp]),
        ("file.ts", &[Mpeg, TypeScript]),
        ("file.MTS", &[Mpeg, TypeScript]),
        ("file.m", &[Matlab, ObjectiveC]),
        ("file.dot", &[Graphviz, Word]),
        ("file.pot", &[Gettext, PowerPoint]),
        ("file.pl", &[Perl, Prolog]),
        ("file.key", &[Der, Keynote, Pem]),
        ("file.wps", &[MicrosoftWorks, WpsWriter]),
    ];
    for &(name, expected) in cases {
        assert_eq!(name_formats(name), expected, "{name}");
    }
    for name in [
        "README",
        ".rs",
        "notes.",
        "notes.unknown",
        "photo.jpg.bak",
        "backup.tar.gz.bak",
        "CMakePresets.json.bak",
        "photo. JPG",
        "photo.jpg ",
        "data.db",
        "data.dat",
        "settings.conf",
        "settings.cfg",
    ] {
        assert!(name_formats(name).is_empty(), "{name}");
    }
}

#[test]
fn known_paths_skip_content_detection() {
    let directory = tempdir().expect("temporary directory");
    let cases: &[(&str, &[FileFormat])] = &[
        ("missing.JPG", &[Jpeg]),
        ("header.h", &[C, Cpp]),
        ("header.H", &[C, Cpp]),
        ("module.d.ts", &[TypeScript]),
        ("Dockerfile", &[Dockerfile]),
        ("package-lock.json", &[Json]),
        ("tsconfig.json", &[Json, TypeScript]),
        ("jsconfig.json", &[JavaScript, Json]),
        ("CMakePresets.json", &[Cmake, Json]),
        ("program.EXE", &[Binary]),
        ("library.dll", &[Binary]),
        ("library.so", &[Binary]),
        ("library.dylib", &[Binary]),
        ("installer.msi", &[Binary]),
        ("package.deb", &[Binary]),
        ("package.rpm", &[Binary]),
        ("package.apk", &[Binary]),
        ("program.elf", &[Binary]),
        ("data.bin", &[Binary]),
    ];
    for &(name, expected) in cases {
        let path = directory.path().join(name);
        assert_eq!(
            FileFormat::from_path(&path).expect("name hint without an existing file"),
            expected,
            "{name}"
        );
    }

    let misleading = directory.path().join("binary.rs");
    fs::write(&misleading, [0, 1, 2, 3]).expect("write sample");
    assert_eq!(
        FileFormat::from_path(&misleading).expect("trusted suffix"),
        [Rust]
    );
}

#[test]
fn unknown_paths_use_content_detection() {
    let directory = tempdir().expect("temporary directory");
    let cases: &[(&str, &[u8], FileFormat)] = &[
        ("image", b"\x89PNG\r\n\x1a\n", Png),
        ("document", b"%PDF-1.7\n", Pdf),
        (
            "script",
            b"#!/usr/bin/env python3\nprint('hello')\n",
            Python,
        ),
        ("README", b"Plain text without an extension.\n", Text),
        ("unexpected.custom", b"plain text", Text),
        ("utf16", b"\xff\xfeh\0i\0\n\0", Text),
        ("encoded", b"-----BEGIN CERTIFICATE-----\nMIIB", Pem),
        ("binary", b"\0\x01\x02\xff", Unknown),
        ("invalid-utf8", b"otherwise readable\xff", Unknown),
        ("empty", b"", Unknown),
        ("whitespace", b" \t\r\n", Unknown),
    ];
    for &(name, bytes, expected) in cases {
        let path = directory.path().join(name);
        fs::write(&path, bytes).expect("write sample");
        assert_eq!(
            FileFormat::from_path(&path).expect("content hint"),
            [expected],
            "{name}"
        );
    }
}

#[test]
fn content_refines_ambiguous_formats() {
    let directory = tempdir().expect("temporary directory");
    let cases: &[(&str, &[u8], &[FileFormat])] = &[
        ("script.pl", b"#!/usr/bin/perl\nprint 1;\n", &[Perl]),
        ("main.ts", b"export const answer = 42;\n", &[TypeScript]),
        ("source.m", b"\0\xff\x01", &[Matlab, ObjectiveC]),
        ("document.dot", b"%PDF-1.7\n", &[Pdf]),
        ("presentation.key", b"PK\x03\x04", &[Keynote]),
        ("private.key", b"-----BEGIN PRIVATE KEY-----\nMIIB", &[Pem]),
    ];
    for &(name, bytes, expected) in cases {
        let path = directory.path().join(name);
        fs::write(&path, bytes).expect("write sample");
        assert_eq!(
            FileFormat::from_path(&path).expect("content hint"),
            expected,
            "{name}"
        );
    }

    let path = directory.path().join("video.ts");
    for (packet_size, offset) in [(188, 0), (192, 4), (204, 0)] {
        let mut bytes = vec![0xff; packet_size * 4];
        for index in 0..4 {
            let start = offset + index * packet_size;
            bytes[start..start + 4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
        }
        fs::write(&path, &bytes).expect("write stream");
        assert_eq!(
            FileFormat::from_path(&path).expect("stream"),
            [Mpeg],
            "packet size: {packet_size}"
        );

        // A partial or inconsistent stream leaves both candidates possible.
        fs::write(&path, &bytes[..packet_size * 3]).expect("write partial stream");
        assert_eq!(
            FileFormat::from_path(&path).expect("partial stream"),
            [Mpeg, TypeScript],
            "packet size: {packet_size}"
        );
        bytes[offset + packet_size] = 0;
        fs::write(&path, &bytes).expect("write inconsistent stream");
        assert_eq!(
            FileFormat::from_path(&path).expect("inconsistent stream"),
            [Mpeg, TypeScript],
            "packet size: {packet_size}"
        );
    }
}

#[test]
fn probing_respects_sample_boundaries() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("sample");
    let mut utf8 = vec![b'a'; HEADER_BYTES - 2];
    utf8.extend_from_slice(b"\xe4\xb8");
    let mut utf16 = vec![0xfe, 0xff];
    for _ in 0..(HEADER_BYTES - 4) / 2 {
        utf16.extend_from_slice(b"\0a");
    }
    utf16.extend_from_slice(b"\xd8\x3e");

    for (encoding, mut bytes, remaining) in [
        ("UTF-8", utf8, &b"\xad"[..]),
        ("UTF-16", utf16, &b"\xdd\x80"[..]),
    ] {
        assert_eq!(bytes.len(), HEADER_BYTES);
        fs::write(&path, &bytes).expect("write incomplete character at EOF");
        assert_eq!(
            FileFormat::from_path(&path).expect("complete file"),
            [Unknown],
            "{encoding}"
        );

        bytes.extend_from_slice(remaining);
        fs::write(&path, &bytes).expect("write character spanning the sample boundary");
        assert_eq!(
            FileFormat::from_path(&path).expect("partial sample"),
            [Text],
            "{encoding}"
        );
    }

    let mut bytes = vec![b'a'; HEADER_BYTES];
    bytes.extend_from_slice(&[0; HEADER_BYTES]);
    for (name, expected) in [
        ("sample", &[Text][..]),
        ("sample.m", &[Matlab, ObjectiveC][..]),
    ] {
        let path = directory.path().join(name);
        fs::write(&path, &bytes).expect("write binary data after the text prefix");
        assert_eq!(
            FileFormat::from_path(&path).expect("prefix hint"),
            expected,
            "{name}"
        );
    }
}

#[test]
fn invalid_paths_report_errors() {
    let directory = tempdir().expect("temporary directory");
    for name in ["missing", "missing.m", "missing.ts"] {
        let path = directory.path().join(name);
        let error = FileFormat::from_path(&path).expect_err("content detection requires a file");
        assert_eq!(error.code(), EngineError::NOT_FOUND, "{name}");
        assert!(error.message().contains(name), "{error}");
        assert!(error.message().contains("detect file format"), "{error}");
    }

    let child = directory.path().join("directory");
    fs::create_dir(&child).expect("create directory");
    let error = FileFormat::from_path(&child).expect_err("not a regular file");
    assert_eq!(error.code(), EngineError::INVALID_ARGUMENT);

    let error = FileFormat::from_path(Path::new("")).expect_err("empty path");
    assert_eq!(error.code(), EngineError::INVALID_ARGUMENT);
}

#[test]
fn file_names_preserve_platform_encodings() {
    #[cfg(unix)]
    {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let directory = tempdir().expect("temporary directory");
        for (name, expected) in [(&b"\xff.JPG"[..], Jpeg), (&b"tsconfig.\xff.json"[..], Json)] {
            let path = directory.path().join(OsString::from_vec(name.to_vec()));
            assert_eq!(
                FileFormat::from_path(&path).expect("extension hint"),
                [expected],
                "{name:?}"
            );
        }
        for name in [b"Dockerfile.\xff".as_slice(), b".env.\xff"] {
            let path = directory.path().join(OsString::from_vec(name.to_vec()));
            FileFormat::from_path(&path).expect_err("unregistered names require file access");
        }

        // The macOS filesystem used for tests rejects non-UTF-8 names on creation.
        #[cfg(target_os = "linux")]
        {
            let text = directory.path().join(OsString::from_vec(b"\xff".to_vec()));
            fs::write(&text, "plain text").expect("write sample");
            assert_eq!(FileFormat::from_path(&text).expect("content hint"), [Text]);
        }
    }
    #[cfg(windows)]
    {
        use std::{ffi::OsString, os::windows::ffi::OsStringExt};

        let name = OsString::from_wide(&[0xd800, 0x2e, 0x4a, 0x50, 0x47]);
        assert_eq!(
            FileFormat::from_path(Path::new(&name)).expect("suffix hint"),
            [Jpeg]
        );
    }
}
