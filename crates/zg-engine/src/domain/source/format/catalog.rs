use zg_engine_macros::file_formats;

use super::FileCategory;

// Ordered alphabetically by canonical name; persistence uses explicit IDs.
// IDs are persistent; never renumber or reuse them.
// Last assigned ID: 154. Add new formats starting at 155 and update this comment.
// Format => (ID, canonical name, categories, extension aliases, exact file names).
file_formats! {
    SevenZip => (1, "7z", [Archive], ["7z"], []),
    Aac => (2, "aac", [Audio], ["aac"], []),
    Access => (3, "access", [Binary], ["accdb", "mdb"], []),
    Aiff => (4, "aiff", [Audio], ["aif", "aiff"], []),
    Arrow => (5, "arrow", [Binary], ["arrow", "arrows", "feather"], []),
    AsciiDoc => (6, "asciidoc", [Document], ["adoc", "asciidoc"], []),
    Assembly => (7, "assembly", [Code], ["asm", "s", "nasm"], []),
    Avi => (8, "avi", [Video], ["avi"], []),
    Avif => (9, "avif", [Image], ["avif"], []),
    Avro => (10, "avro", [Binary], ["avro"], []),
    Bash => (11, "bash", [Code], ["bash"], [".bashrc", ".bash_profile", ".bash_login", ".bash_logout"]),
    Batch => (12, "batch", [Code], ["bat", "cmd"], []),
    Binary => (13, "binary", [Binary], ["exe", "dll", "so", "dylib", "elf", "bin", "msi", "deb", "rpm", "apk", "a", "o"], []),
    Bmp => (14, "bmp", [Image], ["bmp"], []),
    Bzip2 => (15, "bzip2", [Archive], ["bz2"], []),
    C => (16, "c", [Code], ["c", "h"], []),
    Clojure => (17, "clojure", [Code], ["clj", "cljc"], []),
    ClojureScript => (18, "clojurescript", [Code], ["cljs"], []),
    Cmake => (19, "cmake", [Code], ["cmake"], ["CMakeCache.txt", "CMakeLists.txt", "CMakePresets.json", "CMakeUserPresets.json"]),
    Cpp => (20, "cpp", [Code], ["cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++", "h"], []),
    CSharp => (21, "csharp", [Code], ["cs", "csx"], []),
    Css => (22, "css", [Code], ["css"], []),
    Csv => (23, "csv", [Data], ["csv"], []),
    Dart => (24, "dart", [Code], ["dart"], []),
    Der => (25, "der", [Binary], ["der", "key"], []),
    Diff => (26, "diff", [Code], ["diff", "patch"], []),
    Dockerfile => (27, "dockerfile", [Code], ["dockerfile", "containerfile"], ["Dockerfile", "Containerfile", "Dockerfile.dev", "Dockerfile.production", "Containerfile.dev", "Containerfile.production"]),
    Dotenv => (28, "dotenv", [Data], ["env"], [".env", ".flaskenv", ".env.local", ".env.development", ".env.production", ".env.test", ".env.development.local", ".env.production.local", ".env.test.local"]),
    Elixir => (29, "elixir", [Code], ["ex", "exs"], []),
    Eml => (30, "eml", [Document], ["eml"], []),
    Epub => (31, "epub", [Document], ["epub"], []),
    Erlang => (32, "erlang", [Code], ["erl", "hrl"], []),
    Excel => (33, "excel", [Document], ["xls", "xlsb", "xlsm", "xlsx", "xlt", "xltm", "xltx", "xla", "xlam"], []),
    Fish => (34, "fish", [Code], ["fish"], []),
    Flac => (35, "flac", [Audio], ["flac"], []),
    FSharp => (36, "fsharp", [Code], ["fs", "fsx", "fsi"], []),
    Gettext => (37, "gettext", [Data], ["po", "pot"], []),
    Gif => (38, "gif", [Image], ["gif"], []),
    Git => (39, "git", [Data], ["gitignore", "gitattributes", "gitconfig", "gitmodules", "mailmap"], [".gitignore", ".gitattributes", ".gitconfig", ".gitmodules", ".mailmap"]),
    Go => (40, "go", [Code], ["go"], []),
    Graphql => (41, "graphql", [Code], ["graphql", "gql"], []),
    Graphviz => (42, "graphviz", [Code], ["gv", "dot"], []),
    Groovy => (43, "groovy", [Code], ["groovy", "gvy", "gy", "gsh", "gradle"], ["Jenkinsfile"]),
    Gzip => (44, "gzip", [Archive], ["gz"], []),
    Haskell => (45, "haskell", [Code], ["hs", "lhs"], []),
    Hcl => (46, "hcl", [Data], ["hcl"], []),
    Heic => (47, "heic", [Image], ["heic"], []),
    Heif => (48, "heif", [Image], ["heif"], []),
    Html => (49, "html", [Code, Document], ["html", "htm"], []),
    Icalendar => (50, "icalendar", [Data], ["ics", "ical"], []),
    Ico => (51, "ico", [Image], ["ico"], []),
    Illustrator => (52, "illustrator", [Image], ["ai", "ait"], []),
    Ini => (53, "ini", [Data], ["ini"], []),
    Jar => (54, "jar", [Archive, Binary], ["jar"], []),
    Java => (55, "java", [Code], ["java"], []),
    JavaClass => (56, "java-class", [Binary], ["class"], []),
    JavaScript => (57, "javascript", [Code], ["js", "mjs", "cjs", "jsx"], ["jsconfig.json"]),
    Jpeg => (58, "jpeg", [Image], ["jpg", "jpeg", "jpe", "jfif"], []),
    JpegXl => (59, "jpeg-xl", [Image], ["jxl"], []),
    Json => (60, "json", [Data], ["json", "json5", "jsonc"], []),
    JsonLines => (61, "jsonl", [Data], ["jsonl", "ndjson"], []),
    Julia => (62, "julia", [Code], ["jl"], []),
    Jupyter => (63, "jupyter", [Code, Document], ["ipynb"], []),
    Keynote => (64, "keynote", [Document], ["key"], []),
    Kotlin => (65, "kotlin", [Code], ["kt", "kts"], []),
    Latex => (66, "latex", [Code, Document], ["tex"], []),
    Less => (67, "less", [Code], ["less"], []),
    Lua => (68, "lua", [Code], ["lua"], []),
    M4a => (69, "m4a", [Audio], ["m4a"], []),
    M4v => (70, "m4v", [Video], ["m4v"], []),
    Makefile => (71, "makefile", [Code], ["mk", "mak"], ["Makefile", "makefile", "GNUmakefile"]),
    Markdown => (72, "markdown", [Document], ["md", "markdown", "mdown", "mdx"], []),
    Matlab => (73, "matlab", [Code], ["mlx", "m"], []),
    Mhtml => (74, "mhtml", [Document], ["mht", "mhtml"], []),
    MicrosoftWorks => (75, "microsoft-works", [Document], ["wps"], []),
    Mkv => (76, "mkv", [Video], ["mkv"], []),
    Mov => (77, "mov", [Video], ["mov", "qt"], []),
    Mp3 => (78, "mp3", [Audio], ["mp3"], []),
    Mp4 => (79, "mp4", [Video], ["mp4"], []),
    Mpeg => (80, "mpeg", [Video], ["mpeg", "mpg", "m2ts", "mts", "ts"], []),
    Msg => (81, "msg", [Document], ["msg"], []),
    Numbers => (82, "numbers", [Document], ["numbers"], []),
    ObjectiveC => (83, "objective-c", [Code], ["mm", "m"], []),
    Odg => (84, "odg", [Document, Image], ["odg", "otg", "fodg"], []),
    Odp => (85, "odp", [Document], ["odp", "otp", "fodp"], []),
    Ods => (86, "ods", [Document], ["ods", "ots", "fods"], []),
    Odt => (87, "odt", [Document], ["odt", "ott", "fodt"], []),
    Ofd => (88, "ofd", [Document], ["ofd"], []),
    Ogg => (89, "ogg", [Audio, Video], ["ogg", "ogx", "oga", "ogv"], []),
    OneNote => (90, "onenote", [Document], ["one", "onepkg"], []),
    Opus => (91, "opus", [Audio], ["opus"], []),
    Org => (92, "org", [Document], ["org"], []),
    Otf => (93, "otf", [Unknown], ["otf"], []),
    Pages => (94, "pages", [Document], ["pages"], []),
    Parquet => (95, "parquet", [Binary], ["parquet"], []),
    Pdf => (96, "pdf", [Document], ["pdf"], []),
    Pem => (97, "pem", [Binary], ["pem", "key"], []),
    Perl => (98, "perl", [Code], ["perl", "plx", "pm", "psgi", "pl"], []),
    Photoshop => (99, "photoshop", [Image], ["psd", "psb"], []),
    Php => (100, "php", [Code], ["php", "phtml"], []),
    Png => (101, "png", [Image], ["png", "apng"], []),
    PowerPoint => (102, "powerpoint", [Document], ["ppt", "pptm", "pptx", "pps", "ppsm", "ppsx", "potm", "potx", "ppa", "ppam", "pot"], []),
    PowerShell => (103, "powershell", [Code], ["ps1", "psm1", "psd1"], []),
    Prolog => (104, "prolog", [Code], ["prolog", "pl"], []),
    Properties => (105, "properties", [Data], ["properties"], []),
    ProtocolBuffers => (106, "protobuf", [Code], ["proto"], []),
    Python => (107, "python", [Code], ["py", "pyw", "pyi"], []),
    R => (108, "r", [Code], ["r"], []),
    Rar => (109, "rar", [Archive], ["rar"], []),
    Rst => (110, "rst", [Document], ["rst"], []),
    Rtf => (111, "rtf", [Document], ["rtf"], []),
    Ruby => (112, "ruby", [Code], ["rb", "rake"], ["Gemfile", "Rakefile"]),
    Rust => (113, "rust", [Code], ["rs"], []),
    Sass => (114, "sass", [Code], ["sass", "scss"], []),
    Scala => (115, "scala", [Code], ["scala", "sc"], []),
    Shell => (116, "shell", [Code], ["sh"], [".profile"]),
    Sql => (117, "sql", [Code], ["sql"], []),
    Sqlite => (118, "sqlite", [Binary], ["sqlite", "sqlite3"], []),
    Srt => (119, "srt", [Document], ["srt"], []),
    Svelte => (120, "svelte", [Code], ["svelte"], []),
    Svg => (121, "svg", [Image], ["svg", "svgz"], []),
    Swift => (122, "swift", [Code], ["swift"], []),
    Tar => (123, "tar", [Archive], ["tar", "tar.bz2", "tbz", "tbz2", "tar.gz", "tgz", "tar.xz", "txz", "tar.zst", "tar.zstd", "tzst"], []),
    Terraform => (124, "terraform", [Code], ["tf", "tfvars"], []),
    Text => (125, "text", [Document], ["txt", "text", "log"], []),
    Tiff => (126, "tiff", [Image], ["tif", "tiff"], []),
    Toml => (127, "toml", [Data], ["toml"], ["Cargo.lock", "Pipfile", "poetry.lock", "uv.lock"]),
    Tsv => (128, "tsv", [Data], ["tsv"], []),
    Ttf => (129, "ttf", [Unknown], ["ttf"], []),
    TypeScript => (130, "typescript", [Code], ["cts", "tsx", "d.ts", "d.mts", "mts", "ts"], ["tsconfig.json", "tsconfig.build.json", "tsconfig.test.json"]),
    Vcard => (131, "vcard", [Data], ["vcf", "vcard"], []),
    Visio => (132, "visio", [Document], ["vsd", "vdx", "vsdx", "vsdm", "vss", "vsx", "vssx", "vssm", "vst", "vtx", "vstx", "vstm"], []),
    VisualBasic => (133, "visual-basic", [Code], ["vb", "vbs", "vba"], []),
    Vue => (134, "vue", [Code], ["vue"], []),
    Wasm => (135, "wasm", [Binary], ["wasm"], []),
    Wav => (136, "wav", [Audio], ["wav", "wave"], []),
    Webm => (137, "webm", [Video], ["webm"], []),
    Webp => (138, "webp", [Image], ["webp"], []),
    WebVtt => (139, "webvtt", [Document], ["vtt"], []),
    Wma => (140, "wma", [Audio], ["wma"], []),
    Wmv => (141, "wmv", [Video], ["wmv"], []),
    Woff => (142, "woff", [Unknown], ["woff", "woff2"], []),
    Word => (143, "word", [Document], ["doc", "docm", "docx", "dotm", "dotx", "dot"], []),
    WpsPresentation => (144, "wps-presentation", [Document], ["dps", "dpt"], []),
    WpsSpreadsheet => (145, "wps-spreadsheet", [Document], ["et", "ett"], []),
    WpsWriter => (146, "wps-writer", [Document], ["wpt", "wps"], []),
    Xml => (147, "xml", [Data], ["xml", "xsd", "xsl", "xslt"], []),
    Xps => (148, "xps", [Document], ["xps"], []),
    Xz => (149, "xz", [Archive], ["xz"], []),
    Yaml => (150, "yaml", [Data], ["yaml", "yml"], []),
    Zig => (151, "zig", [Code], ["zig", "zig.zon"], []),
    Zip => (152, "zip", [Archive], ["zip"], []),
    Zsh => (153, "zsh", [Code], ["zsh"], [".zshrc", ".zprofile", ".zshenv", ".zlogin", ".zlogout"]),
    Zstd => (154, "zstd", [Archive], ["zst", "zstd"], []),
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
    mod registered_aliases {
        use zg_engine_macros::file_formats;

        use super::super::FileCategory;

        file_formats! {
            Alpha => (7, "alpha", [Code], ["alpha", "shared"], ["KnownFile", "KnownConfig", "SharedFile"]),
            Beta => (42, "beta", [Document], ["b", "beta", "shared", "shared.long"], ["SharedFile"]),
            Gamma => (3, "gamma", [Code, Document], ["g", "shared.long"], []),
        }

        #[test]
        fn generates_formats_and_looks_up_their_registered_names() {
            assert_eq!(FileFormat::Unknown.as_str(), "unknown");
            assert_eq!(FileFormat::Alpha.as_str(), "alpha");
            assert_eq!(FileFormat::Beta.as_str(), "beta");
            assert_eq!(FileFormat::Gamma.as_str(), "gamma");

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
        fn numeric_ids_follow_assignments_instead_of_declaration_order() {
            assert_eq!(FileFormat::Unknown as u16, 0);
            assert_eq!(FileFormat::Alpha as u16, 7);
            assert_eq!(FileFormat::Beta as u16, 42);
            assert_eq!(FileFormat::Gamma as u16, 3);
        }

        #[test]
        fn unregistered_ids_have_no_format() {
            assert_eq!(FileFormat::from_id(1), None);
            assert_eq!(FileFormat::from_id(u16::MAX), None);
        }

        #[test]
        fn every_format_round_trips_through_its_id() {
            for &format in FORMATS {
                assert_eq!(FileFormat::from_id(format as u16), Some(format));
            }
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
