use super::runtime_path;
use std::{fs, path::Path};

fn library_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "libpdfium.dylib"
    } else if cfg!(windows) {
        "pdfium.dll"
    } else {
        "libpdfium.so"
    }
}

fn write(path: &Path) {
    fs::create_dir_all(path.parent().expect("parent")).expect("directory");
    fs::write(path, b"fixture").expect("file");
}

#[test]
fn explicit_pdfium_directory_does_not_require_executable_lookup() {
    let directory = std::path::PathBuf::from("custom/pdfium");
    assert_eq!(
        runtime_path(Some(directory.clone()), Path::new("nonexistent/zg")).expect("override"),
        directory.join(library_name()),
    );
}

#[test]
fn pdfium_lookup_preserves_packaged_and_cargo_layouts() {
    for relative in ["bin", "deps", ""] {
        let root = tempfile::tempdir().expect("root");
        let executable = root.path().join(relative).join("zg");
        let library = if relative == "bin" {
            root.path().join("lib").join(library_name())
        } else {
            root.path().join(library_name())
        };
        write(&executable);
        write(&library);
        assert_eq!(
            runtime_path(None, &executable)
                .expect("runtime")
                .canonicalize()
                .expect("real runtime"),
            library.canonicalize().expect("library"),
        );
    }
}

#[cfg(unix)]
#[test]
fn npm_symlink_uses_packaged_pdfium_instead_of_global_prefix() {
    let root = tempfile::tempdir().expect("prefix");
    let package = root.path().join("lib/node_modules/@zvec/zvec-grep");
    let executable = package.join("bin/zg");
    let library = package.join("lib").join(library_name());
    let entry = root.path().join("bin/zg");
    write(&executable);
    write(&library);
    write(&root.path().join("bin").join(library_name()));
    std::os::unix::fs::symlink(&executable, &entry).expect("npm entry");
    assert_eq!(
        runtime_path(None, &entry)
            .expect("runtime")
            .canonicalize()
            .expect("real runtime"),
        library.canonicalize().expect("packaged library"),
    );
}
