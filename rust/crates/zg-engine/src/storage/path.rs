//! Lossless identity keys and optional Unicode query projections are separate.

use std::{
    ffi::{OsStr, OsString},
    path::{Component, Path, PathBuf},
};

use crate::{EngineError, EngineResult, domain::SourcePath};

use super::zvec::{native, scalar, wildcard_string};
use crate::domain::{DirectoryId, FileRecord};
use serde::{Deserialize, Serialize};
use zvec_rust::{CollectionSchema, DataType, Doc};

pub(super) fn encode_path(path: &SourcePath) -> EngineResult<String> {
    serde_json::to_string(&PathRecord::from_path(path)?).map_err(|error| {
        EngineError::storage_failure(format!("cannot encode identity path: {error}"))
    })
}

pub(super) fn decode_path(value: &str) -> EngineResult<SourcePath> {
    let record: PathRecord = serde_json::from_str(value).map_err(|error| {
        EngineError::storage_failure(format!("cannot decode identity path: {error}"))
    })?;
    SourcePath::new(record.into_path()?)
}

/// Hex preserves the portable identity representation and needs no SQL escaping.
pub(super) fn path_key(path: &SourcePath) -> EngineResult<String> {
    Ok(hex::encode(encode_path(path)?.as_bytes()))
}

/// This projection accelerates path matching; it never establishes identity.
pub(super) fn query_path(path: &Path) -> Option<String> {
    let value = path.to_str()?;
    #[cfg(windows)]
    return Some(value.replace('\\', "/"));
    #[cfg(not(windows))]
    Some(value.to_owned())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PathRecord {
    components: Vec<PathComponent>,
}

impl PathRecord {
    pub(crate) fn from_path(path: &Path) -> EngineResult<Self> {
        let path = SourcePath::new(path)?;
        let components = path
            .components()
            .map(|component| PathComponent::from_component(component.as_os_str()))
            .collect::<EngineResult<_>>()?;
        Ok(Self { components })
    }

    pub(crate) fn into_path(self) -> EngineResult<PathBuf> {
        let mut path = PathBuf::new();
        for component in self.components {
            let component = component.into_component()?;
            validate_component(&component)?;
            path.push(component);
        }
        Ok(SourcePath::new(path)?.into_path_buf())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum PathComponent {
    Utf8(String),
    Native(NativeComponent),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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

pub(super) fn file_membership_schema(schema: &mut CollectionSchema) -> EngineResult<()> {
    scalar(
        schema,
        "ancestor_directory_ids",
        DataType::ArrayUint32,
        false,
        true,
    )?;
    wildcard_string(schema, "file_name", false)
}

pub(super) fn file_membership_doc(
    doc: &mut Doc,
    file: &FileRecord,
    directories: &[DirectoryId],
) -> EngineResult<()> {
    native(
        doc.add_array_u32(
            "ancestor_directory_ids",
            &directories.iter().map(|id| id.get()).collect::<Vec<_>>(),
        ),
        "encode ancestor directories",
    )?;
    // Non-Unicode names have no STRING representation. The source-path cache disables name
    // pushdown in this workspace; exact native paths remain in FileRecord.
    let name = file
        .relative_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    native(doc.add_string("file_name", name), "encode file name")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_components_keep_identity_across_native_separator_spellings() {
        let encoded = r#"{"components":["images","中文 房间","living.jpg"]}"#;
        let path = decode_path(encoded).expect("portable fixture");
        assert_eq!(
            path.as_path(),
            Path::new("images").join("中文 房间").join("living.jpg")
        );
        assert_eq!(
            encode_path(&path).expect("encode on this platform"),
            encoded
        );
        assert_eq!(
            path_key(&path).expect("identity"),
            hex::encode(encoded.as_bytes())
        );
        assert_eq!(
            query_path(&path),
            Some("images/中文 房间/living.jpg".into())
        );
    }

    #[test]
    fn rejects_old_records_and_components_that_change_path_structure() {
        for encoded in [
            r#"{"encoding":"utf8","value":"images/living.jpg"}"#,
            r#"{"components":[]}"#,
            r#"{"components":[""]}"#,
            r#"{"components":["."]}"#,
            r#"{"components":["..","outside"]}"#,
            r#"{"components":["images/living.jpg"]}"#,
            r#"{"components":["name\u0000"]}"#,
            r#"{"components":[{"encoding":"unix_bytes","value":[97]}]}"#,
        ] {
            assert!(decode_path(encoded).is_err(), "{encoded}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_components_preserve_backslashes_and_non_unicode_without_splitting() {
        use std::os::unix::ffi::OsStringExt;
        let path =
            SourcePath::new(Path::new("rooms").join(OsString::from_vec(b"name\\\xff".to_vec())))
                .expect("native path");
        let encoded = encode_path(&path).expect("encode native component");
        assert_eq!(
            decode_path(&encoded).expect("restore native component"),
            path
        );
        assert!(
            decode_path(r#"{"components":[{"encoding":"windows_wide","value":[55296]}]}"#).is_err()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_components_preserve_non_unicode_and_reject_unix_names() {
        use std::os::windows::ffi::OsStringExt;
        let path = SourcePath::new(Path::new("rooms").join(OsString::from_wide(&[0xd800])))
            .expect("native path");
        let encoded = encode_path(&path).expect("encode native component");
        assert_eq!(
            decode_path(&encoded).expect("restore native component"),
            path
        );
        assert!(decode_path(r#"{"components":["name\\part"]}"#).is_err());
        assert!(
            decode_path(r#"{"components":[{"encoding":"unix_bytes","value":[255]}]}"#).is_err()
        );
    }

    #[test]
    fn windows_filename_validation_rejects_aliases_and_unrepresentable_names() {
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
        }
        for value in ["living.jpg", "中文 房间", "COM10.txt"] {
            validate_windows_component(value.as_bytes()).expect("portable Windows filename");
        }
    }
}
