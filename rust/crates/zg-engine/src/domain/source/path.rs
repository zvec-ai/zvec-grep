use std::{
    ffi::{OsStr, OsString},
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{EngineError, EngineResult};

/// A validated native path relative to a workspace root.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SourcePath(PathBuf);

impl SourcePath {
    #[track_caller]
    pub(crate) fn new(path: impl Into<PathBuf>) -> EngineResult<Self> {
        let path = path.into();
        Self::validate(&path)?;
        Ok(Self(path))
    }

    pub(crate) fn as_path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn into_path_buf(self) -> PathBuf {
        self.0
    }

    fn validate(path: &Path) -> EngineResult<()> {
        if path.as_os_str().is_empty()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            || path.as_os_str().as_encoded_bytes().contains(&0)
        {
            return Err(EngineError::invalid_argument(
                "source path must be a relative file or directory path within the workspace",
            ));
        }
        let normalized: PathBuf = path.components().collect();
        #[cfg(windows)]
        let same_spelling = normalized
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .map(|byte| if *byte == b'/' { b'\\' } else { *byte })
            .eq(path
                .as_os_str()
                .as_encoded_bytes()
                .iter()
                .map(|byte| if *byte == b'/' { b'\\' } else { *byte }));
        #[cfg(not(windows))]
        let same_spelling = normalized.as_os_str() == path.as_os_str();
        if !same_spelling {
            return Err(EngineError::invalid_argument(
                "source path must use normalized native path components",
            ));
        }
        Ok(())
    }
}

impl AsRef<Path> for SourcePath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

// Only immutable Path operations are exposed; mutation requires constructing
// another SourcePath and validating it again.
impl std::ops::Deref for SourcePath {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.as_path()
    }
}

impl Serialize for SourcePath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        EncodedPath::from_path(self)
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SourcePath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        EncodedPath::deserialize(deserializer)?
            .into_path()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EncodedPath {
    components: Vec<PathComponent>,
}

impl EncodedPath {
    fn from_path(path: &SourcePath) -> EngineResult<Self> {
        let components = path
            .components()
            .map(|component| PathComponent::from_component(component.as_os_str()))
            .collect::<EngineResult<_>>()?;
        Ok(Self { components })
    }

    fn into_path(self) -> EngineResult<SourcePath> {
        let mut path = PathBuf::new();
        for component in self.components {
            let component = component.into_component()?;
            validate_component(&component)?;
            path.push(component);
        }
        SourcePath::new(path)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum PathComponent {
    Utf8(String),
    Native(NativeComponent),
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "encoding",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum NativeComponent {
    UnixBytes(Vec<u8>),
    WindowsWide(Vec<u16>),
}

impl PathComponent {
    fn from_component(component: &OsStr) -> EngineResult<Self> {
        validate_component(component)?;
        if let Some(value) = component.to_str() {
            return Ok(Self::Utf8(value.to_owned()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Ok(Self::Native(NativeComponent::UnixBytes(
                component.as_bytes().to_vec(),
            )))
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Ok(Self::Native(NativeComponent::WindowsWide(
                component.encode_wide().collect(),
            )))
        }
        #[cfg(not(any(unix, windows)))]
        Err(EngineError::storage_failure(
            "cannot store a non-Unicode path on this platform",
        ))
    }

    fn into_component(self) -> EngineResult<OsString> {
        match self {
            Self::Utf8(value) => Ok(OsString::from(value)),
            Self::Native(NativeComponent::UnixBytes(bytes)) => {
                if std::str::from_utf8(&bytes).is_ok() {
                    return Err(invalid_component("non-canonical Unix byte encoding"));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStringExt;
                    Ok(OsString::from_vec(bytes))
                }
                #[cfg(not(unix))]
                Err(invalid_component(
                    "Unix bytes cannot be represented on this platform",
                ))
            }
            Self::Native(NativeComponent::WindowsWide(units)) => {
                if String::from_utf16(&units).is_ok() {
                    return Err(invalid_component("non-canonical Windows wide encoding"));
                }
                #[cfg(windows)]
                {
                    use std::os::windows::ffi::OsStringExt;
                    Ok(OsString::from_wide(&units))
                }
                #[cfg(not(windows))]
                Err(invalid_component(
                    "Windows wide characters cannot be represented on this platform",
                ))
            }
        }
    }
}

fn validate_component(value: &OsStr) -> EngineResult<()> {
    let mut components = Path::new(value).components();
    if value.as_encoded_bytes().contains(&0)
        || !matches!(components.next(), Some(Component::Normal(component)) if component == value)
        || components.next().is_some()
    {
        return Err(invalid_component("not a single normal filename"));
    }
    #[cfg(windows)]
    validate_windows_component(value.as_encoded_bytes())?;
    Ok(())
}

#[cfg(any(windows, test))]
fn validate_windows_component(value: &[u8]) -> EngineResult<()> {
    let base = value
        .split(|byte| *byte == b'.')
        .next()
        .unwrap_or_default()
        .trim_ascii_end();
    let reserved = [
        b"CON".as_slice(),
        b"PRN",
        b"AUX",
        b"NUL",
        b"CONIN$",
        b"CONOUT$",
    ]
    .iter()
    .any(|name| base.eq_ignore_ascii_case(name))
        || (base.len() >= 4
            && (base[..3].eq_ignore_ascii_case(b"COM") || base[..3].eq_ignore_ascii_case(b"LPT"))
            && (matches!(&base[3..], [b'1'..=b'9'])
                || ["¹", "²", "³"]
                    .iter()
                    .any(|digit| &base[3..] == digit.as_bytes())));
    if reserved
        || value.last().is_some_and(|byte| matches!(byte, b'.' | b' '))
        || value
            .iter()
            .any(|byte| *byte < 32 || b"<>:\"/\\|?*".contains(byte))
    {
        return Err(invalid_component(
            "filename cannot be represented safely on Windows",
        ));
    }
    Ok(())
}

fn invalid_component(reason: &str) -> EngineError {
    EngineError::invalid_argument(format!("invalid stored path component: {reason}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_paths_round_trip() {
        let common: &[(&str, &[&str])] = &[
            ("file.rs", &["file.rs"]),
            ("src/file.rs", &["src", "file.rs"]),
            ("with space/file name.rs", &["with space", "file name.rs"]),
            ("src/.hidden", &["src", ".hidden"]),
            ("中文/文件", &["中文", "文件"]),
            ("한국어/日本語.txt", &["한국어", "日本語.txt"]),
            (
                "images/中文 房间/living.jpg",
                &["images", "中文 房间", "living.jpg"],
            ),
        ];
        let mut cases: Vec<_> = common
            .iter()
            .map(|(path, components)| {
                (
                    PathBuf::from(path),
                    serde_json::json!({"components": components}),
                )
            })
            .collect();

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            for (path, components) in [
                (r"src\name/file.rs", [r"src\name", "file.rs"]),
                (r"src/\..\file", ["src", r"\..\file"]),
                ("with space / file.rs ", ["with space ", " file.rs "]),
            ] {
                cases.push((path.into(), serde_json::json!({"components": components})));
            }
            for (parent, bytes) in [
                ("src", b"invalid-\xff".as_slice()),
                ("rooms", b"name\\\xff".as_slice()),
            ] {
                cases.push((
                    Path::new(parent).join(OsString::from_vec(bytes.to_vec())),
                    serde_json::json!({"components": [
                        parent, {"encoding": "unix_bytes", "value": bytes}
                    ]}),
                ));
            }
        }

        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            for path in [
                r"src\engine\file.rs",
                "src/engine/file.rs",
                r"src\engine/file.rs",
            ] {
                cases.push((
                    path.into(),
                    serde_json::json!({"components": ["src", "engine", "file.rs"]}),
                ));
            }
            cases.push((
                Path::new("src").join(OsString::from_wide(&[0xd800])),
                serde_json::json!({"components": [
                    "src", {"encoding": "windows_wide", "value": [0xd800]}
                ]}),
            ));
        }

        for (path, expected) in cases {
            let source = SourcePath::new(&path).expect("valid path");
            assert_eq!(source.as_os_str(), path.as_os_str());
            let encoded = serde_json::to_value(&source).expect("encode path");
            assert_eq!(encoded, expected, "{path:?}");
            let restored: SourcePath = serde_json::from_value(encoded).expect("decode path");
            assert_eq!(restored, source, "{path:?}");
            assert_eq!(source.into_path_buf().as_os_str(), path.as_os_str());
        }
    }

    #[test]
    fn rejects_invalid_paths() {
        for path in [
            "",
            ".",
            "..",
            "/",
            "/src/file.rs",
            "./src/file.rs",
            "../file.rs",
            "src/../file.rs",
            "src/./file.rs",
            "src//file.rs",
            "src/file.rs/",
            "src/file.rs/.",
            "src/bad\0name",
        ] {
            assert!(SourcePath::new(path).is_err(), "{path:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn rejects_invalid_windows_paths() {
        for path in [
            r"C:\src\file.rs",
            r"C:src\file.rs",
            r"\src\file.rs",
            r"\\server\share\file.rs",
            r"src\\file.rs",
            r"src\.\file.rs",
            r"src\/file.rs",
        ] {
            assert!(SourcePath::new(path).is_err(), "{path:?}");
        }
    }

    #[test]
    fn rejects_invalid_encoded_paths() {
        let mut cases = vec![
            r#"{"components":[]}"#,
            r#"{"components":[""]}"#,
            r#"{"components":["."]}"#,
            r#"{"components":["..","outside"]}"#,
            r#"{"components":["images/living.jpg"]}"#,
            r#"{"components":["name\u0000"]}"#,
            r#"{"components":[{"encoding":"unix_bytes","value":[97]}]}"#,
        ];
        #[cfg(unix)]
        cases.push(r#"{"components":[{"encoding":"windows_wide","value":[55296]}]}"#);
        #[cfg(windows)]
        cases.extend([
            r#"{"components":["name\\part"]}"#,
            r#"{"components":[{"encoding":"unix_bytes","value":[255]}]}"#,
        ]);
        for encoded in cases {
            assert!(
                serde_json::from_str::<SourcePath>(encoded).is_err(),
                "{encoded}"
            );
        }
    }

    #[test]
    fn validates_windows_filenames() {
        for value in [
            "CON",
            "nul.txt",
            "COM1.txt",
            "COM¹.txt",
            "CON .txt",
            "LPT9",
            "name.",
            "name ",
            "a:b",
            "a\\b",
            "a/b",
        ] {
            assert!(
                validate_windows_component(value.as_bytes()).is_err(),
                "{value}"
            );
            #[cfg(windows)]
            assert!(
                serde_json::from_value::<SourcePath>(serde_json::json!({"components": [value]}))
                    .is_err(),
                "{value}"
            );
        }
        for value in ["living.jpg", "中文 房间", "COM10.txt"] {
            validate_windows_component(value.as_bytes()).expect("portable Windows filename");
            #[cfg(windows)]
            assert_eq!(
                serde_json::from_value::<SourcePath>(serde_json::json!({"components": [value]}))
                    .expect("decode valid Windows filename")
                    .as_path(),
                Path::new(value)
            );
        }
    }
}
