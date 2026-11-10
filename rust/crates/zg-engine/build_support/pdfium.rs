//! Build/package dependency only. PDF extraction never downloads native code
//! or invokes external tools at runtime.
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
fn hash(path: &Path) -> Result<String> {
    let mut value = String::with_capacity(64);
    for byte in Sha256::digest(fs::read(path)?) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 15)]));
    }
    Ok(value)
}

pub fn prepare() -> Result<()> {
    let manifest =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/pdfium-distributions.json");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rerun-if-changed=build_support/pdfium.rs");
    for name in ["ZG_PDFIUM_CACHE_DIR", "CARGO_NET_OFFLINE"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let config: serde_json::Value = serde_json::from_slice(&fs::read(manifest)?)?;
    let target = env::var("TARGET")?;
    let asset = &config["targets"][&target];
    let get = |name: &str| {
        asset[name]
            .as_str()
            .ok_or_else(|| format!("Missing pinned PDFium {name} for {target}"))
    };
    let archive_name = get("archive")?;
    let expected = get("sha256")?;
    let library = get("library")?;
    let output = PathBuf::from(env::var_os("OUT_DIR").ok_or("Missing Cargo OUT_DIR")?);
    let profile = output
        .ancestors()
        .nth(3)
        .ok_or("Invalid Cargo output directory")?;
    let cache = env::var_os("ZG_PDFIUM_CACHE_DIR")
        .map_or_else(
            || profile.parent().unwrap_or(profile).join("pdfium-prebuilt"),
            PathBuf::from,
        )
        .join(config["version"].as_str().ok_or("Missing PDFium version")?);
    fs::create_dir_all(&cache)?;
    let archive = cache.join(archive_name);
    if !archive.is_file() || hash(&archive)? != expected {
        if env::var("CARGO_NET_OFFLINE").is_ok_and(|s| s == "1" || s.eq_ignore_ascii_case("true")) {
            return Err(format!(
                "Offline build requires verified PDFium archive {}",
                archive.display()
            )
            .into());
        }
        let temporary = tempfile::NamedTempFile::new_in(&cache)?;
        let status = Command::new(if cfg!(windows) { "curl.exe" } else { "curl" })
            .args([
                "-fL",
                "--retry",
                "3",
                "--retry-connrefused",
                "--connect-timeout",
                "20",
                "--max-time",
                "180",
                "-o",
            ])
            .arg(temporary.path())
            .arg(get("url")?)
            .status()?;
        if !status.success() {
            return Err("PDFium archive download failed".into());
        }
        if hash(temporary.path())? != expected {
            return Err("PDFium archive SHA-256 mismatch".into());
        }
        temporary.persist(&archive)?;
    }
    stage(&archive, library, profile)
}

fn stage(archive: &Path, library: &str, profile: &Path) -> Result<()> {
    let mut package = tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(archive)?));
    let mut binary = None;
    let mut notices = std::collections::BTreeMap::new();
    for entry in package.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.to_string_lossy().into_owned();
        if path == library || path == "LICENSE" || path.starts_with("licenses/") {
            if entry.size() > 128 * 1024 * 1024 {
                return Err("Oversized PDFium archive entry".into());
            }
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            if path == library {
                if binary.replace(bytes).is_some() {
                    return Err("Duplicate PDFium library".into());
                }
            } else {
                notices.insert(path, bytes);
            }
        }
    }
    let binary = binary.ok_or("PDFium archive lacks selected library")?;
    if !notices.contains_key("LICENSE") || notices.len() < 2 {
        return Err("PDFium archive lacks license notices".into());
    }
    let mut combined = Vec::new();
    for (name, bytes) in notices {
        writeln!(combined, "----- {name} -----")?;
        combined.extend(bytes);
        combined.push(b'\n');
    }
    let filename = Path::new(library)
        .file_name()
        .ok_or("Invalid PDFium library path")?;
    for directory in [
        profile.to_path_buf(),
        profile.join("deps"),
        profile.join("examples"),
    ] {
        publish(&directory.join(filename), &binary)?;
        publish(
            &directory.join("licenses/pdfium-ThirdPartyNotices.txt"),
            &combined,
        )?;
    }
    Ok(())
}
fn publish(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or("Invalid PDFium destination")?;
    fs::create_dir_all(parent)?;
    if !fs::read(path).is_ok_and(|existing| existing == bytes) {
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o644))?;
        }
        temporary.persist(path)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o644))?;
    }
    println!("cargo:rerun-if-changed={}", path.display());
    Ok(())
}
