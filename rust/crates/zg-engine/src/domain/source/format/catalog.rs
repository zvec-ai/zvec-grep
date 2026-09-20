use zg_engine_macros::file_formats;

use super::FileCategory;

// Ordered alphabetically by canonical name.
// Format => (canonical name, categories, extension aliases, exact file names).
// Aliases are case-sensitive: register each supported spelling explicitly.
file_formats! {
    SevenZip => ("7z", [Archive], ["7z", "7Z"], []),
    Aac => ("aac", [Audio], ["aac", "AAC", "Aac"], []),
    Access => ("access", [Binary], ["accdb", "mdb"], []),
    Aiff => ("aiff", [Audio], ["aif", "aiff", "AIF", "AIFF", "aifc", "AIFC", "Aif", "Aiff", "Aifc"], []),
    Arrow => ("arrow", [Binary], ["arrow", "arrows", "feather"], []),
    AsciiDoc => ("asciidoc", [Document], ["adoc", "asciidoc"], []),
    Assembly => ("assembly", [Code], ["asm", "s", "S", "nasm"], []),
    Avi => ("avi", [Video], ["avi", "AVI", "Avi"], []),
    Avif => ("avif", [Image], ["avif", "AVIF", "Avif"], []),
    Avro => ("avro", [Binary], ["avro"], []),
    Bash => ("bash", [Code], ["bash"], [".bashrc", ".bash_profile", ".bash_login", ".bash_logout", ".bash_aliases", "bashrc", "bash_profile", "bash_login", "bash_logout"]),
    Batch => ("batch", [Code], ["bat", "cmd"], []),
    Binary => ("binary", [Binary], ["exe", "EXE", "dll", "DLL", "so", "dylib", "elf", "bin", "msi", "MSI", "deb", "rpm", "apk", "a", "o", "Exe", "Dll", "Msi"], []),
    Bmp => ("bmp", [Image], ["bmp", "BMP", "dib", "DIB", "Bmp", "Dib"], []),
    Bzip2 => ("bzip2", [Archive], ["bz2"], []),
    C => ("c", [Code], ["c", "h"], []),
    Clojure => ("clojure", [Code], ["clj", "cljc"], []),
    ClojureScript => ("clojurescript", [Code], ["cljs"], []),
    Cmake => ("cmake", [Code], ["cmake"], ["CMakeCache.txt", "CMakeLists.txt", "CMakePresets.json", "CMakeUserPresets.json"]),
    Cpp => ("cpp", [Code], ["C", "cc", "cpp", "cxx", "c++", "H", "hh", "hpp", "hxx", "h++", "h", "inl", "cppm", "ccm", "cxxm", "c++m", "ixx"], []),
    CSharp => ("csharp", [Code], ["cs", "csx"], []),
    Css => ("css", [Code], ["css"], []),
    Csv => ("csv", [Data], ["csv"], []),
    Dart => ("dart", [Code], ["dart"], []),
    Der => ("der", [Binary], ["der", "key"], []),
    Diff => ("diff", [Code], ["diff", "patch"], []),
    Dockerfile => ("dockerfile", [Code], ["dockerfile", "containerfile"], ["Dockerfile", "Containerfile", "Dockerfile.dev", "Dockerfile.production", "Containerfile.dev", "Containerfile.production"]),
    Dotenv => ("dotenv", [Data], ["env"], [".env", ".flaskenv", ".env.local", ".env.development", ".env.production", ".env.test", ".env.development.local", ".env.production.local", ".env.test.local"]),
    Elixir => ("elixir", [Code], ["ex", "exs"], []),
    Eml => ("eml", [Document], ["eml"], []),
    Epub => ("epub", [Document], ["epub"], []),
    Erlang => ("erlang", [Code], ["erl", "hrl"], []),
    Excel => ("excel", [Document], ["xls", "xlsb", "xlsm", "xlsx", "xlt", "xltm", "xltx", "xla", "xlam", "XLS", "XLSB", "XLSM", "XLSX", "XLT", "XLTM", "XLTX", "XLA", "XLAM", "Xls", "Xlsb", "Xlsm", "Xlsx", "Xlt", "Xltm", "Xltx", "Xla", "Xlam"], []),
    Fish => ("fish", [Code], ["fish"], []),
    Flac => ("flac", [Audio], ["flac", "FLAC", "Flac"], []),
    FSharp => ("fsharp", [Code], ["fs", "fsx", "fsi"], []),
    Gettext => ("gettext", [Data], ["po", "pot"], []),
    Gif => ("gif", [Image], ["gif", "GIF", "Gif"], []),
    Git => ("git", [Data], ["gitignore", "gitattributes", "gitconfig", "gitmodules", "mailmap"], [".gitignore", ".gitattributes", ".gitconfig", ".gitmodules", ".mailmap"]),
    Go => ("go", [Code], ["go"], []),
    Graphql => ("graphql", [Code], ["graphql", "gql", "graphqls"], []),
    Graphviz => ("graphviz", [Code], ["gv", "dot"], []),
    Groovy => ("groovy", [Code], ["groovy", "gvy", "gy", "gsh", "gradle"], ["Jenkinsfile"]),
    Gzip => ("gzip", [Archive], ["gz"], []),
    Haskell => ("haskell", [Code], ["hs", "lhs", "cpphs", "c2hs", "hsc"], []),
    Hcl => ("hcl", [Data], ["hcl"], []),
    Heic => ("heic", [Image], ["heic", "HEIC", "Heic"], []),
    Heif => ("heif", [Image], ["heif", "HEIF", "Heif"], []),
    Html => ("html", [Code, Document], ["html", "htm"], []),
    Icalendar => ("icalendar", [Data], ["ics", "ical"], []),
    Ico => ("ico", [Image], ["ico", "ICO", "Ico"], []),
    Illustrator => ("illustrator", [Image], ["ai", "ait", "AI", "AIT", "Ai", "Ait"], []),
    Ini => ("ini", [Data], ["ini"], [".editorconfig"]),
    Jar => ("jar", [Archive, Binary], ["jar"], []),
    Java => ("java", [Code], ["java"], []),
    JavaClass => ("java-class", [Binary], ["class"], []),
    JavaScript => ("javascript", [Code], ["js", "mjs", "cjs", "jsx"], ["jsconfig.json"]),
    Jpeg => ("jpeg", [Image], ["jpg", "jpeg", "jpe", "jfif", "JPG", "JPEG", "JPE", "JFIF", "Jpg", "Jpeg", "Jpe", "Jfif"], []),
    JpegXl => ("jpeg-xl", [Image], ["jxl", "JXL", "Jxl"], []),
    Json => ("json", [Data], ["json", "json5", "jsonc", "sarif"], ["composer.lock", "Pipfile.lock"]),
    JsonLines => ("jsonl", [Data], ["jsonl", "ndjson"], []),
    Julia => ("julia", [Code], ["jl"], []),
    Jupyter => ("jupyter", [Code, Document], ["ipynb"], []),
    Keynote => ("keynote", [Document], ["key"], []),
    Kotlin => ("kotlin", [Code], ["kt", "kts"], []),
    Latex => ("latex", [Code, Document], ["tex", "ltx", "cls", "sty", "dtx", "ins"], []),
    Less => ("less", [Code], ["less"], []),
    Lua => ("lua", [Code], ["lua"], []),
    M4a => ("m4a", [Audio], ["m4a", "M4A", "M4a"], []),
    M4v => ("m4v", [Video], ["m4v", "M4V", "M4v"], []),
    Makefile => ("makefile", [Code], ["mk", "mak"], ["Makefile", "makefile", "GNUmakefile", "Makefile.am", "makefile.am", "GNUmakefile.am", "Makefile.in", "makefile.in", "GNUmakefile.in"]),
    Markdown => ("markdown", [Document], ["md", "MD", "markdown", "mdown", "mdwn", "mkd", "mkdn", "mdx", "Md"], []),
    Matlab => ("matlab", [Code], ["mlx", "m"], []),
    Mhtml => ("mhtml", [Document], ["mht", "mhtml"], []),
    MicrosoftWorks => ("microsoft-works", [Document], ["wps"], []),
    Mkv => ("mkv", [Video], ["mkv", "MKV", "Mkv"], []),
    Mov => ("mov", [Video], ["mov", "qt", "MOV", "QT", "Mov", "Qt"], []),
    Mp3 => ("mp3", [Audio], ["mp3", "MP3", "Mp3"], []),
    Mp4 => ("mp4", [Video], ["mp4", "MP4", "Mp4"], []),
    Mpeg => ("mpeg", [Video], ["mpeg", "mpg", "m2ts", "mts", "ts", "MPEG", "MPG", "M2TS", "MTS", "TS", "Mpeg", "Mpg", "M2ts", "Mts", "Ts"], []),
    Msg => ("msg", [Document], ["msg"], []),
    Numbers => ("numbers", [Document], ["numbers"], []),
    ObjectiveC => ("objective-c", [Code], ["mm", "m"], []),
    Odg => ("odg", [Document, Image], ["odg", "otg", "fodg"], []),
    Odp => ("odp", [Document], ["odp", "otp", "fodp"], []),
    Ods => ("ods", [Document], ["ods", "ots", "fods"], []),
    Odt => ("odt", [Document], ["odt", "ott", "fodt"], []),
    Ofd => ("ofd", [Document], ["ofd"], []),
    Ogg => ("ogg", [Audio, Video], ["ogg", "ogx", "oga", "ogv", "OGG", "OGX", "OGA", "OGV", "Ogg", "Ogx", "Oga", "Ogv"], []),
    OneNote => ("onenote", [Document], ["one", "onepkg"], []),
    Opus => ("opus", [Audio], ["opus", "OPUS", "Opus"], []),
    Org => ("org", [Document], ["org", "org_archive"], []),
    Otf => ("otf", [Unknown], ["otf"], []),
    Pages => ("pages", [Document], ["pages"], []),
    Parquet => ("parquet", [Binary], ["parquet"], []),
    Pdf => ("pdf", [Document], ["pdf", "PDF", "Pdf"], []),
    Pem => ("pem", [Binary], ["pem", "key"], []),
    Perl => ("perl", [Code], ["perl", "plx", "pm", "psgi", "pl", "PL", "plh"], []),
    Photoshop => ("photoshop", [Image], ["psd", "psb", "PSD", "PSB", "Psd", "Psb"], []),
    Php => ("php", [Code], ["php", "phtml", "php3", "php4", "php5", "php7", "php8", "pht"], []),
    Png => ("png", [Image], ["png", "apng", "PNG", "APNG", "Png", "Apng"], []),
    PowerPoint => ("powerpoint", [Document], ["ppt", "pptm", "pptx", "pps", "ppsm", "ppsx", "potm", "potx", "ppa", "ppam", "pot", "PPT", "PPTM", "PPTX", "PPS", "PPSM", "PPSX", "POTM", "POTX", "PPA", "PPAM", "POT", "Ppt", "Pptm", "Pptx", "Pps", "Ppsm", "Ppsx", "Potm", "Potx", "Ppa", "Ppam", "Pot"], []),
    PowerShell => ("powershell", [Code], ["ps1", "psm1", "psd1"], []),
    Prolog => ("prolog", [Code], ["prolog", "pl"], []),
    Properties => ("properties", [Data], ["properties"], []),
    ProtocolBuffers => ("protobuf", [Code], ["proto"], []),
    Python => ("python", [Code], ["py", "pyw", "pyi"], []),
    R => ("r", [Code], ["r", "R", "Rmd", "rmd", "Rnw", "rnw"], []),
    Rar => ("rar", [Archive], ["rar", "RAR", "Rar"], []),
    Rst => ("rst", [Document], ["rst"], []),
    Rtf => ("rtf", [Document], ["rtf"], []),
    Ruby => ("ruby", [Code], ["rb", "rbw", "rake", "gemspec"], ["Gemfile", "Rakefile", "config.ru", ".irbrc"]),
    Rust => ("rust", [Code], ["rs"], []),
    Sass => ("sass", [Code], ["sass", "scss"], []),
    Scala => ("scala", [Code], ["scala", "sc", "sbt"], []),
    Shell => ("shell", [Code], ["sh"], [".profile", "profile"]),
    Sql => ("sql", [Code], ["sql", "psql"], []),
    Sqlite => ("sqlite", [Binary], ["sqlite", "sqlite3"], []),
    Srt => ("srt", [Document], ["srt"], []),
    Svelte => ("svelte", [Code], ["svelte"], []),
    Svg => ("svg", [Image], ["svg", "svgz", "SVG", "SVGZ", "Svg", "Svgz"], []),
    Swift => ("swift", [Code], ["swift"], []),
    Tar => ("tar", [Archive], ["tar", "tar.bz2", "tbz", "tbz2", "tar.gz", "tgz", "tar.xz", "txz", "tar.zst", "tar.zstd", "tzst"], []),
    Terraform => ("terraform", [Code], ["tf", "tfvars"], []),
    Text => ("text", [Document], ["txt", "TXT", "text", "log", "Txt"], []),
    Tiff => ("tiff", [Image], ["tif", "tiff", "TIF", "TIFF", "Tif", "Tiff"], []),
    Toml => ("toml", [Data], ["toml"], ["Cargo.lock", "Pipfile", "poetry.lock", "uv.lock"]),
    Tsv => ("tsv", [Data], ["tsv"], []),
    Ttf => ("ttf", [Unknown], ["ttf"], []),
    TypeScript => ("typescript", [Code], ["cts", "tsx", "d.ts", "d.cts", "d.mts", "mts", "ts"], ["tsconfig.json", "tsconfig.build.json", "tsconfig.test.json"]),
    Vcard => ("vcard", [Data], ["vcf", "vcard"], []),
    Visio => ("visio", [Document], ["vsd", "vdx", "vsdx", "vsdm", "vss", "vsx", "vssx", "vssm", "vst", "vtx", "vstx", "vstm"], []),
    VisualBasic => ("visual-basic", [Code], ["vb", "vbs", "vba"], []),
    Vue => ("vue", [Code], ["vue"], []),
    Wasm => ("wasm", [Binary], ["wasm"], []),
    Wav => ("wav", [Audio], ["wav", "wave", "WAV", "WAVE", "Wav", "Wave"], []),
    Webm => ("webm", [Video], ["webm", "WEBM", "Webm"], []),
    Webp => ("webp", [Image], ["webp", "WEBP", "Webp"], []),
    WebVtt => ("webvtt", [Document], ["vtt"], []),
    Wma => ("wma", [Audio], ["wma", "WMA", "Wma"], []),
    Wmv => ("wmv", [Video], ["wmv", "WMV", "Wmv"], []),
    Woff => ("woff", [Unknown], ["woff", "woff2"], []),
    Word => ("word", [Document], ["doc", "docm", "docx", "dotm", "dotx", "dot", "DOC", "DOCM", "DOCX", "DOTM", "DOTX", "DOT", "Doc", "Docm", "Docx", "Dotm", "Dotx", "Dot"], []),
    WpsPresentation => ("wps-presentation", [Document], ["dps", "dpt"], []),
    WpsSpreadsheet => ("wps-spreadsheet", [Document], ["et", "ett"], []),
    WpsWriter => ("wps-writer", [Document], ["wpt", "wps"], []),
    Xml => ("xml", [Data], ["xml", "xsd", "xsl", "xslt", "xml.dist", "xjb", "rng", "sch"], []),
    Xps => ("xps", [Document], ["xps"], []),
    Xz => ("xz", [Archive], ["xz"], []),
    Yaml => ("yaml", [Data], ["yaml", "yml"], [".clang-format", "_clang-format", ".clang-tidy"]),
    Zig => ("zig", [Code], ["zig", "zig.zon"], []),
    Zip => ("zip", [Archive], ["zip", "ZIP", "pyz", "pyzw", "Zip"], []),
    Zsh => ("zsh", [Code], ["zsh"], [".zshrc", ".zprofile", ".zshenv", ".zlogin", ".zlogout", "zshrc", "zprofile", "zshenv", "zlogin", "zlogout"]),
    Zstd => ("zstd", [Archive], ["zst", "zstd"], []),
}

/// Some extensions legitimately match multiple formats and need no further probing.
pub(super) fn needs_sniff(formats: &[FileFormat]) -> bool {
    formats.len() > 1
        && !formats
            .iter()
            .all(|format| matches!(format, FileFormat::C | FileFormat::Cpp))
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats_serialize_by_canonical_name() {
        for &format in super::FileFormat::ALL {
            let json = serde_json::to_value(format).expect("format JSON");
            assert_eq!(json, format.as_str());
            assert_eq!(
                serde_json::from_value::<super::FileFormat>(json).expect("format round trip"),
                format
            );
        }
        assert_eq!(
            serde_json::from_str::<super::FileFormat>(r#""JPG""#).expect("extension alias"),
            super::FileFormat::Jpeg
        );
        assert!(serde_json::from_str::<super::FileFormat>("58").is_err());
        assert!(serde_json::from_str::<super::FileFormat>(r#""unregistered""#).is_err());
    }

    mod registered_aliases {
        use zg_engine_macros::file_formats;

        use super::super::FileCategory;

        file_formats! {
            Alpha => ("alpha", [Code], ["alpha", "shared"], ["KnownFile", "KnownConfig", "SharedFile"]),
            Beta => ("beta", [Document], ["b", "beta", "shared", "shared.long"], ["SharedFile"]),
            Gamma => ("gamma", [Code, Document], ["g", "shared.long"], []),
        }

        #[test]
        fn generates_formats_and_looks_up_their_registered_names() {
            assert_eq!(FileFormat::Unknown.as_str(), "unknown");
            assert_eq!(FileFormat::Alpha.as_str(), "alpha");
            assert_eq!(FileFormat::Beta.as_str(), "beta");
            assert_eq!(FileFormat::Gamma.as_str(), "gamma");
            assert_eq!(
                FileFormat::ALL,
                &[
                    FileFormat::Unknown,
                    FileFormat::Alpha,
                    FileFormat::Beta,
                    FileFormat::Gamma
                ]
            );
            assert!(FileFormat::Unknown.extensions().is_empty());
            assert!(FileFormat::Unknown.file_names().is_empty());
            assert_eq!(
                FileFormat::Beta.extensions(),
                &["b", "beta", "shared", "shared.long"]
            );
            assert_eq!(
                FileFormat::Alpha.file_names(),
                &["KnownFile", "KnownConfig", "SharedFile"]
            );

            assert_eq!(FileFormat::Unknown.categories(), &[FileCategory::Unknown]);
            assert_eq!(FileFormat::Alpha.categories(), &[FileCategory::Code]);
            assert_eq!(FileFormat::Beta.categories(), &[FileCategory::Document]);
            assert_eq!(
                FileFormat::Gamma.categories(),
                &[FileCategory::Code, FileCategory::Document]
            );

            assert_eq!(lookup_extension("alpha"), &[FileFormat::Alpha]);
            assert_eq!(lookup_extension("b"), &[FileFormat::Beta]);
            assert_eq!(lookup_extension("beta"), &[FileFormat::Beta]);
            assert_eq!(lookup_extension("g"), &[FileFormat::Gamma]);
            assert_eq!(lookup_name("KnownFile"), &[FileFormat::Alpha]);
            assert_eq!(lookup_name("KnownConfig"), &[FileFormat::Alpha]);

            assert_eq!(MAX_EXTENSION_LEN, "shared.long".len());
        }

        #[test]
        fn shared_aliases_return_all_registered_candidates() {
            assert_eq!(
                lookup_name("SharedFile"),
                &[FileFormat::Alpha, FileFormat::Beta]
            );
            assert_eq!(
                lookup_extension("shared"),
                &[FileFormat::Alpha, FileFormat::Beta]
            );
            assert_eq!(
                lookup_extension("shared.long"),
                &[FileFormat::Beta, FileFormat::Gamma]
            );
        }

        #[test]
        fn every_format_round_trips_through_its_canonical_name() {
            for &format in FileFormat::ALL {
                assert_eq!(FileFormat::parse(format.as_str()), Some(format));
            }
            assert_eq!(FileFormat::parse(" .b "), Some(FileFormat::Beta));
            assert_eq!(FileFormat::parse(" .B "), None);
            assert_eq!(FileFormat::parse(" BETA "), Some(FileFormat::Beta));
            assert_eq!(FileFormat::parse("shared"), None);
            assert_eq!(FileFormat::parse("unregistered"), None);
        }

        #[test]
        fn names_and_extensions_must_be_explicitly_registered() {
            assert_eq!(lookup_name("alpha"), &[]);
            assert_eq!(lookup_extension("gamma"), &[]);
            assert_eq!(lookup_name("UnregisteredFile"), &[]);
            assert_eq!(lookup_extension("unregistered"), &[]);
        }
    }
}
