//! Deploy zg's load-time libraries, not a policy for other SDK consumers.
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Deserialize)]
struct Manifest {
    schema: u32,
    target: String,
    linkage: String,
    link_libraries: Vec<String>,
    files: BTreeMap<String, String>,
}

fn sha256(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let mut digest = String::with_capacity(64);
    for byte in hash.finalize() {
        write!(digest, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(digest)
}

fn runtime_name(filename: &str, library: &str, windows: bool) -> bool {
    if windows {
        filename == format!("{library}.dll")
    } else {
        let soname = format!("lib{library}.so");
        filename == soname || filename.starts_with(&format!("{soname}."))
    }
}

fn deploy(source: &Path, filename: &str, digest: &str, profile: &Path) -> Result<Vec<PathBuf>> {
    let modified = fs::metadata(source)?.modified()?;
    let mut staged = Vec::new();
    for directory in [
        profile.to_path_buf(),
        profile.join("deps"),
        profile.join("examples"),
    ] {
        fs::create_dir_all(&directory)?;
        let destination = directory.join(filename);
        let unchanged = fs::symlink_metadata(&destination).is_ok_and(|metadata| metadata.is_file())
            && sha256(&destination)? == digest;
        if !unchanged {
            // Copy bytes, not links. Atomic replacement also repairs dangling
            // aliases left by the source wrapper without following those links.
            let temporary = tempfile::NamedTempFile::new_in(&directory)?;
            fs::copy(source, temporary.path())?;
            if sha256(temporary.path())? != digest {
                return Err(format!("llama.cpp runtime changed while copying: {filename}").into());
            }
            // Watched outputs retain the input mtime to avoid a rebuild loop.
            temporary.as_file().set_modified(modified)?;
            temporary.persist(&destination)?;
        }
        staged.push(destination);
    }
    Ok(staged)
}

/// Deploy source-built core libraries using only the current dependency's
/// `DEP_LLAMA_BACKENDS_DIR`. CPU/CUDA plugins stay in that backend directory.
///
/// # Errors
/// Rejects missing cores, conflicting aliases and links outside this build's
/// output. Never falls back to scanning another build or an SDK cache.
pub fn stage_source(backends: &Path, target: &str, out: &Path) -> Result<Vec<PathBuf>> {
    if !(target.contains("linux") || target.contains("windows")) {
        return Err("unsupported llama.cpp source runtime target".into());
    }
    if backends.file_name().and_then(|name| name.to_str()) != Some("backends")
        || !fs::symlink_metadata(backends)?.is_dir()
    {
        return Err("invalid llama.cpp source backend directory".into());
    }
    let root = backends
        .parent()
        .ok_or("missing source output root")?
        .canonicalize()?;
    let profile = out.ancestors().nth(3).ok_or("invalid Cargo OUT_DIR")?;
    let libraries = ["llama", "ggml", "ggml-base"];
    let mut files: BTreeMap<String, (PathBuf, String)> = BTreeMap::new();
    for directory in ["lib", "lib64", "bin"] {
        let entries = match fs::read_dir(root.join(directory)) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !libraries
                .iter()
                .any(|library| runtime_name(name, library, target.contains("windows")))
            {
                continue;
            }
            // Resolve .so -> .so.0 -> versioned-file chains before copying.
            let source = path.canonicalize()?;
            if !source.starts_with(&root) || !source.is_file() {
                return Err(format!("invalid llama.cpp source runtime: {}", path.display()).into());
            }
            let digest = sha256(&source)?;
            if files
                .get(name)
                .is_some_and(|(_, previous)| previous != &digest)
            {
                return Err(format!("conflicting llama.cpp source runtime: {name}").into());
            }
            files.insert(name.to_owned(), (source, digest));
        }
    }
    for library in libraries {
        if !files
            .keys()
            .any(|name| runtime_name(name, library, target.contains("windows")))
        {
            return Err(format!("missing source runtime for llama.cpp library: {library}").into());
        }
    }
    let mut staged = Vec::new();
    for (name, (source, digest)) in files {
        staged.extend(deploy(&source, &name, &digest, profile)?);
        staged.push(source);
    }
    Ok(staged)
}

/// Consume only Cargo's direct-dependency SDK metadata. Never scan build caches
/// for a different ABI or copy GPU plugins into the load-time search directory.
///
/// # Errors
/// Returns an error for invalid SDK metadata, corrupt libraries or failed copies.
pub fn stage(root: &Path, manifest_path: &Path, target: &str, out: &Path) -> Result<Vec<PathBuf>> {
    let root = root.canonicalize()?;
    if manifest_path.canonicalize()?.parent() != Some(root.as_path()) {
        return Err("llama.cpp runtime manifest does not belong to its SDK directory".into());
    }
    let manifest: Manifest = serde_json::from_slice(&fs::read(manifest_path)?)?;
    if manifest.schema != 1 || manifest.target != target {
        return Err("llama.cpp runtime manifest schema/target mismatch".into());
    }
    if manifest.linkage == "static" {
        return Ok(Vec::new());
    }
    if manifest.linkage != "dynamic" || !(target.contains("linux") || target.contains("windows")) {
        return Err("unsupported llama.cpp shared runtime layout".into());
    }
    let profile = out.ancestors().nth(3).ok_or("invalid Cargo OUT_DIR")?;
    let libraries: Vec<_> = manifest
        .link_libraries
        .iter()
        .filter(|library| !library.starts_with("static="))
        .filter_map(|library| library.rsplit('=').next())
        .filter(|name| name.starts_with("llama") || name.starts_with("ggml"))
        .collect();
    let mut staged = Vec::new();
    for library in &libraries {
        // The pinned SDK's common-base CMake target is always STATIC. On
        // Windows its .lib is nevertheless recorded as dylib= (MSVC uses .lib
        // for both static and import libraries). Do not invent a DLL for this
        // archive, but still require DLLs for every other dynamic link entry.
        if target.contains("windows")
            && *library == "llama-common-base"
            && manifest.files.contains_key("lib/llama-common-base.lib")
            && !manifest.files.contains_key("lib/llama-common-base.dll")
        {
            continue;
        }
        let mut found = false;
        for (name, digest) in &manifest.files {
            let Some(filename) = name.strip_prefix("lib/") else {
                continue;
            };
            if filename.contains(['/', '\\', ':'])
                || !runtime_name(filename, library, target.contains("windows"))
            {
                continue;
            }
            found = true;
            let source = root.join(name);
            let source_metadata = fs::symlink_metadata(&source)?;
            if !source_metadata.is_file() || sha256(&source)? != *digest {
                return Err(format!("invalid llama.cpp runtime payload: {name}").into());
            }
            staged.extend(deploy(&source, filename, digest, profile)?);
        }
        if !found {
            return Err(
                format!("missing shared runtime for llama.cpp link library: {library}").into(),
            );
        }
    }
    if staged.is_empty() {
        return Err("llama.cpp dynamic SDK contains no load-time libraries".into());
    }
    Ok(staged)
}
