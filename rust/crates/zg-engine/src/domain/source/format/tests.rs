use std::{
    collections::{HashMap, HashSet},
    fs,
};

use tempfile::tempdir;

use super::*;
use FileFormat::*;

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
        (Eps, "eps", &[Category::Image]),
        (Svg, "svg", &[Category::Image]),
        (Mp3, "mp3", &[Category::Audio]),
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
    let mut names = HashSet::new();
    for &format in FileFormat::ALL {
        let name = format.as_str();
        assert!(!name.is_empty());
        assert_eq!(name, name.trim());
        assert_eq!(name, name.to_ascii_lowercase());
        assert!(names.insert(name), "duplicate name: {name}");
        assert_eq!(FileFormat::parse(name), Some(format));
        let json = serde_json::to_value(format).expect("format JSON");
        assert_eq!(json, name);
        assert_eq!(
            serde_json::from_value::<FileFormat>(json).expect("format round trip"),
            format
        );
    }

    let mut extensions = HashMap::new();
    for &format in FileFormat::ALL {
        for &extension in format.extensions() {
            let candidates = extensions.entry(extension).or_insert_with(Vec::new);
            assert!(
                !candidates.contains(&format),
                "duplicate registration: {extension}, {format:?}"
            );
            candidates.push(format);
        }
    }
    for (extension, mut candidates) in extensions {
        candidates.sort_unstable();
        let name = format!("sample.{extension}");
        assert_eq!(
            FileFormat::from_file_name(name.as_ref()),
            candidates,
            "{name}"
        );
    }

    let mut file_names = HashMap::new();
    for &format in FileFormat::ALL {
        for &name in format.file_names() {
            let candidates = file_names.entry(name).or_insert_with(Vec::new);
            assert!(
                !candidates.contains(&format),
                "duplicate registration: {name}, {format:?}"
            );
            candidates.push(format);
        }
    }
    for (name, candidates) in file_names {
        assert_eq!(catalog::lookup_name(name), candidates, "{name}");
        let formats = FileFormat::from_file_name(name.as_ref());
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
        (".bash_aliases", &[Bash]),
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
        assert_eq!(
            FileFormat::from_file_name(name.as_ref()),
            expected,
            "{name}"
        );
    }
    for name in [
        "dockerfile",
        "Dockerfile.custom",
        "Containerfile.custom",
        ".env.custom",
        "dockerfile.dev",
        ".ENV.local",
    ] {
        assert_eq!(
            FileFormat::from_file_name(name.as_ref()),
            [Unknown],
            "{name}"
        );
    }

    let mut formats = vec![TypeScript, Json, Unknown, Json, Text];
    normalize_formats(&mut formats);
    assert_eq!(formats, [Json, TypeScript]);
}

#[test]
fn extensions_match_longest_registered_suffix() {
    let aliases: &[(&[&str], FileFormat)] = &[
        (&["eps"], Eps),
        (&["jpg", "JPEG", "jfif", "Jpg", "Jpeg", "Jpe", "Jfif"], Jpeg),
        (&["doc", "DOCX", "docm", "dotx"], Word),
        (&["xls", "XLSX", "xlsb"], Excel),
        (&["ppt", "pptx", "potm"], PowerPoint),
        (&["json", "jsonc", "json5"], Json),
        (&["js", "jsx", "mjs", "cjs"], JavaScript),
        (
            &["md", "MD", "markdown", "mdwn", "mkd", "mkdn", "mdx"],
            Markdown,
        ),
        (&["txt", "TXT"], Text),
        (&["zip", "ZIP", "pyz", "pyzw"], Zip),
        (&["rar", "RAR"], Rar),
        (&["7z", "7Z"], SevenZip),
        (&["odg", "otg", "fodg"], Odg),
        (&["ogg", "oga", "ogv"], Ogg),
        (&["woff", "woff2"], Woff),
    ];
    for &(extensions, expected) in aliases {
        for extension in extensions {
            let name = format!("sample.{extension}");
            assert_eq!(
                FileFormat::from_file_name(name.as_ref()),
                [expected],
                "{name}"
            );
        }
    }
    let cases: &[(&str, &[FileFormat])] = &[
        ("scan.TIF", &[Tiff]),
        ("report.最终.PDF", &[Pdf]),
        (".config.json", &[Json]),
        ("backup.2026.tar.gz", &[Tar]),
        ("events.json.gz", &[Gzip]),
        ("data.notar.gz", &[Gzip]),
        (".tar.gz", &[Gzip]),
        ("module.d.ts", &[TypeScript]),
        ("module.d.cts", &[TypeScript]),
        ("module.d.mts", &[TypeScript]),
        ("module.D.MTS", &[Unknown]),
        ("header.H", &[Cpp]),
        ("file.ts", &[TypeScript]),
        ("file.mts", &[TypeScript]),
        ("file.MTS", &[Unknown]),
        ("video.mpeg", &[Unknown]),
        ("video.m2ts", &[Unknown]),
        ("file.m", &[Matlab, ObjectiveC]),
        ("file.dot", &[Graphviz, Word]),
        ("file.pot", &[Gettext, PowerPoint]),
        ("file.pl", &[Perl, Prolog]),
        ("file.key", &[Der, Keynote, Pem]),
        ("file.wps", &[MicrosoftWorks, WpsWriter]),
    ];
    for &(name, expected) in cases {
        assert_eq!(
            FileFormat::from_file_name(name.as_ref()),
            expected,
            "{name}"
        );
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
        "scan.TiF",
        "photo.jPg",
        "photo.pNg",
        "main.RS",
        "main.Rs",
        ".config.JSON",
        "backup.2026.TAR.GZ",
    ] {
        assert_eq!(
            FileFormat::from_file_name(name.as_ref()),
            [Unknown],
            "{name}"
        );
    }
}

#[test]
fn canonical_names_and_extension_aliases_have_separate_case_rules() {
    let cases = [
        ("Rust", Some(Rust)),
        ("RUST", Some(Rust)),
        ("rs", Some(Rust)),
        (" .rs ", Some(Rust)),
        ("RS", None),
        (".RS", None),
        (".rust", None),
        ("C", Some(C)),
        (".c", Some(C)),
        (".C", Some(Cpp)),
        (".h", None),
        (".H", Some(Cpp)),
        ("JPEG", Some(Jpeg)),
        ("JPG", Some(Jpeg)),
        (".JPG", Some(Jpeg)),
        ("Jpg", Some(Jpeg)),
        (".Jpeg", Some(Jpeg)),
        (".jPg", None),
        (".R", Some(R)),
        (".S", Some(Assembly)),
    ];
    for (input, expected) in cases {
        assert_eq!(FileFormat::parse(input), expected, "{input}");
    }
}

#[test]
fn uppercase_language_suffixes_are_explicit_catalog_entries() {
    for (name, expected) in [
        ("main.rs", Rust),
        ("main.c", C),
        ("main.C", Cpp),
        ("main.H", Cpp),
        ("main.s", Assembly),
        ("main.S", Assembly),
        ("analysis.r", R),
        ("analysis.R", R),
        ("analysis.Rmd", R),
        ("analysis.Rnw", R),
        ("template.inl", Cpp),
        ("library.gemspec", Ruby),
        ("config.ru", Ruby),
        (".irbrc", Ruby),
        ("Makefile.am", Makefile),
        ("GNUmakefile.in", Makefile),
    ] {
        assert_eq!(
            FileFormat::from_file_name(name.as_ref()),
            [expected],
            "{name}"
        );
    }
    for name in [
        "main.RS",
        "main.CPP",
        "main.ASM",
        "analysis.RMD",
        "CONFIG.RU",
    ] {
        assert_eq!(
            FileFormat::from_file_name(name.as_ref()),
            [Unknown],
            "{name}"
        );
    }
}

#[test]
fn catalog_matches_do_not_probe_content() {
    let directory = tempdir().expect("temporary directory");
    let cases: &[(&str, &[FileFormat])] = &[
        ("image.eps", &[Eps]),
        ("image.rs", &[Rust]),
        ("source.rs", &[Rust]),
        ("source.h", &[C, Cpp]),
        ("Dockerfile", &[Dockerfile]),
        ("tsconfig.json", &[Json, TypeScript]),
        ("notes.md", &[Markdown]),
        ("wrong-script.rs", &[Rust]),
        ("missing.eps", &[Eps]),
    ];
    for &(name, expected) in cases {
        let path = directory.path().join(name);
        let bytes: &[u8] = match name {
            "image.eps" => b"%!PS-Adobe-3.0 EPSF-3.0\n",
            "image.rs" => b"\x89PNG\r\n\x1a\n",
            "wrong-script.rs" => b"#!/bin/sh\necho hello\n",
            _ => b"fn main() {}\n",
        };
        if name != "missing.eps" {
            fs::write(&path, bytes).expect("write sample");
        }
        assert_eq!(
            FileFormat::from_path(&path).expect("catalog match"),
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
fn unmatched_paths_only_detect_shebang_scripts() {
    let directory = tempdir().expect("temporary directory");
    let cases: &[(&str, &[u8], FileFormat)] = &[
        ("image", b"\x89PNG\r\n\x1a\n", Unknown),
        ("document", b"%PDF-1.7\n", Unknown),
        (
            "script",
            b"#!/usr/bin/env python3\nprint('hello')\n",
            Python,
        ),
        ("script.custom", b"#!/bin/sh\necho hello\n", Unknown),
        (
            "unsupported-script",
            b"#!/usr/bin/env awk\nBEGIN {}\n",
            Unknown,
        ),
        ("hashbang-text", b"not a script\n#!/bin/sh\n", Unknown),
        ("README", b"Plain text without an extension.\n", Unknown),
        ("unexpected.custom", b"plain text", Unknown),
        ("main.RS", b"fn main() {}\n", Unknown),
        ("image.RS", b"\x89PNG\r\n\x1a\n", Unknown),
        (
            "drawing.EPS",
            b"%!PS-Adobe-3.0 EPSF-3.0\n%%BoundingBox: 0 0 1 1\n",
            Unknown,
        ),
        ("drawing.custom", b"%!PS-Adobe-3.0 EPSF-3.0\n", Unknown),
        ("utf16", b"\xff\xfeh\0i\0\n\0", Unknown),
        ("encoded", b"-----BEGIN CERTIFICATE-----\nMIIB", Unknown),
        ("binary", b"\0\x01\x02\xff", Unknown),
        ("binary.custom", b"\0\x01\x02\xff", Unknown),
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
fn catalog_ambiguity_does_not_probe_content() {
    let directory = tempdir().expect("temporary directory");
    for (name, expected) in [
        ("main.ts", &[TypeScript][..]),
        ("source.m", &[Matlab, ObjectiveC][..]),
        ("private.key", &[Der, Keynote, Pem][..]),
    ] {
        let path = directory.path().join(name);
        assert_eq!(
            FileFormat::from_path(&path).expect("catalog match"),
            expected,
            "{name}"
        );
    }
}

#[test]
fn probing_respects_sample_boundaries() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("script");
    let mut bytes = b"#!/bin/sh\n".to_vec();
    bytes.resize(HEADER_BYTES - 2, b'a');
    bytes.extend_from_slice(b"\xe4\xb8");
    assert_eq!(bytes.len(), HEADER_BYTES);
    fs::write(&path, &bytes).expect("write incomplete character at EOF");
    assert_eq!(
        FileFormat::from_path(&path).expect("complete file"),
        [Unknown]
    );

    bytes.extend_from_slice(b"\xad");
    fs::write(&path, &bytes).expect("write character spanning the sample boundary");
    assert_eq!(
        FileFormat::from_path(&path).expect("partial sample"),
        [Shell]
    );
}

#[test]
fn invalid_paths_report_errors() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("missing");
    let error =
        FileFormat::from_path(&path).expect_err("suffixless file requires content detection");
    assert_eq!(error.code(), EngineError::NOT_FOUND);
    assert!(error.message().contains("missing"), "{error}");
    assert!(error.message().contains("detect file format"), "{error}");

    assert_eq!(
        FileFormat::from_path(&directory.path().join("missing.rs")).expect("catalog"),
        [Rust]
    );
    assert_eq!(
        FileFormat::from_path(&directory.path().join("missing.RS")).expect("unknown suffix"),
        [Unknown]
    );

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
            let name = OsString::from_vec(name.to_vec());
            assert_eq!(match_longest_extension(&name), [expected]);
            #[cfg(target_os = "linux")]
            {
                let bytes: &[u8] = if expected == Jpeg {
                    b"\xff\xd8\xff"
                } else {
                    b"{}"
                };
                let path = directory.path().join(name);
                fs::write(&path, bytes).expect("write sample");
                assert_eq!(
                    FileFormat::from_path(&path).expect("validated extension"),
                    [expected]
                );
            }
        }
        for name in [b"Dockerfile.\xff".as_slice(), b".env.\xff"] {
            let path = directory.path().join(OsString::from_vec(name.to_vec()));
            assert_eq!(
                FileFormat::from_path(&path).expect("unknown suffix"),
                [Unknown]
            );
        }

        // The macOS filesystem used for tests rejects non-UTF-8 names on creation.
        #[cfg(target_os = "linux")]
        {
            let text = directory.path().join(OsString::from_vec(b"\xff".to_vec()));
            fs::write(&text, "plain text").expect("write sample");
            assert_eq!(
                FileFormat::from_path(&text).expect("content hint"),
                [Unknown]
            );
        }
    }
    #[cfg(windows)]
    {
        use std::{ffi::OsString, os::windows::ffi::OsStringExt};

        let name = OsString::from_wide(&[0xd800, 0x2e, 0x4a, 0x50, 0x47]);
        assert_eq!(match_longest_extension(&name), [Jpeg]);
    }
}
