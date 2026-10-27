//! Tests for zg-owned deployment from Cargo's verified SDK metadata.
#[path = "../llama_runtime_build.rs"]
mod llama_runtime;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fmt::Write as _, fs, path::Path};

// Exercise the exact shared preparer, using tiny local archives, not a second
// Rust implementation of the Python extraction/download contract.
const ORT_DEPLOYMENT_CHECK: &str = r"
import importlib.util, io, os, subprocess, sys, tarfile, tempfile, zipfile
from pathlib import Path
from unittest.mock import patch
spec = importlib.util.spec_from_file_location('prepare_ort', sys.argv[1])
ort = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ort)

def archive_at(path, platform, files):
    if platform == 'linux':
        with tarfile.open(path, 'w:gz') as archive:
            for name in files:
                data = name.encode()
                entry = tarfile.TarInfo('bundle/' + name)
                entry.size = len(data)
                entry.mtime = 1700000000
                archive.addfile(entry, io.BytesIO(data))
    else:
        with zipfile.ZipFile(path, 'w') as archive:
            for name in files:
                entry = zipfile.ZipInfo('bundle/' + name, (2023, 11, 14, 0, 0, 0))
                archive.writestr(entry, name.encode())

with tempfile.TemporaryDirectory() as temporary:
    root = Path(temporary)
    for platform in ('linux', 'win32'):
        cache = root / platform / 'ort-cuda12'
        cache.mkdir(parents=True)
        archive = cache / ('fixture.tgz' if platform == 'linux' else 'fixture.zip')
        wanted = {**ort.required_files(platform), 'LICENSE': 'licenses/onnxruntime-LICENSE',
                  'ThirdPartyNotices.txt': 'licenses/onnxruntime-ThirdPartyNotices.txt'}
        names = [*wanted, '../../unwanted-file']
        archive_at(archive, platform, names)
        checksum = ort.digest(archive)
        ort.DISTRIBUTIONS[platform] = (archive.name, checksum)
        with patch.object(ort.subprocess, 'run', side_effect=AssertionError('unexpected download')):
            bundle = root / platform / 'npm-bundle/lib'
            ort.prepare(cache, bundle, platform, offline=True)
            assert {p.relative_to(bundle).as_posix() for p in bundle.rglob('*') if p.is_file()} == set(wanted.values())
            for profile_name in ('debug', 'release', 'target-triple/custom-profile'):
                profile = root / platform / profile_name
                def stage():
                    ort.prepare(cache, profile, platform, cargo=True, offline=True)
                stage()
                for directory in (profile, profile / 'deps', profile / 'examples'):
                    for source, name in wanted.items():
                        assert (directory / name).read_bytes() == source.encode()
                        assert not (directory / name).is_symlink()
                    assert not (directory / 'unwanted-file').exists()
                files = [profile / name for name in wanted.values()]
                mtimes = [p.stat().st_mtime_ns for p in files]
                stage()
                assert mtimes == [p.stat().st_mtime_ns for p in files]
                files[0].unlink()
                files[1].write_bytes(b'corrupt copy')
                unrelated = profile / 'user-file'
                unrelated.write_bytes(b'keep')
                stage()
                assert mtimes == [p.stat().st_mtime_ns for p in files]
                assert unrelated.read_bytes() == b'keep'
                assert ort.digest(archive) == checksum
                if os.name != 'nt':
                    files[0].unlink()
                    files[0].symlink_to(unrelated)
                    stage()
                    assert not files[0].is_symlink()
                    assert unrelated.read_bytes() == b'keep'
            # Integrity errors must not touch already-deployed files or fetch
            # in offline mode. This includes incomplete and duplicate payloads.
            bad = cache / archive.name.replace('fixture', 'invalid')
            for entries in (names[:-2], [*names, names[0]]):
                archive_at(bad, platform, entries)
                before = [p.read_bytes() for p in files]
                try:
                    ort.extract_runtime(bad, profile, platform, ort.digest(bad), cargo=True)
                except RuntimeError:
                    pass
                else:
                    raise AssertionError('invalid archive accepted')
                assert before == [p.read_bytes() for p in files]
            archive.write_bytes(b'corrupt cache')
            try:
                stage()
            except RuntimeError as error:
                assert 'Offline build requires' in str(error)
            else:
                raise AssertionError('corrupt archive accepted')
        # CLI routing must honor the current (possibly cross-target) OUT_DIR,
        # never fall back to downloading when an explicit offline cache misses.
        out = root / platform / 'isolated/triple/debug/build/zg-hash/out'
        result = subprocess.run([sys.executable, sys.argv[1], '--platform', platform,
                                 '--cargo-out-dir', str(out)], capture_output=True, text=True,
                                env={**os.environ, 'CARGO_NET_OFFLINE': 'true'})
        assert result.returncode != 0
        assert 'Offline build requires' in result.stderr
        assert str(out.parents[3] / 'ort-cuda12') in result.stderr
";

#[test]
fn ort_cargo_deployment_is_verified_repeatable_and_repairs_outputs_on_both_platforms() {
    let python = std::env::var_os("PYTHON")
        .unwrap_or_else(|| if cfg!(windows) { "python" } else { "python3" }.into());
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/cuda/prepare-onnxruntime.py");
    let result = std::process::Command::new(python)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg("-c")
        .arg(ORT_DEPLOYMENT_CHECK)
        .arg(script)
        .output()
        .expect("Python 3 for the shared ORT preparer tests");
    assert!(
        result.status.success(),
        "ORT preparer test failed:\n{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

const CARGO_CLI_REGRESSION_CHECK: &str = r"
import json, os, subprocess, sys, tarfile, tempfile
from contextlib import ExitStack
from pathlib import Path, PurePosixPath
from unittest.mock import patch
sys.path.insert(0, sys.argv[1])
import runtime, e2e

previous_directory = Path.cwd()
with tempfile.TemporaryDirectory() as temporary, ExitStack() as cleanup:
    # Windows cannot remove the process's current directory. Restore it before
    # TemporaryDirectory cleans up, including when an assertion fails.
    cleanup.callback(os.chdir, previous_directory)
    os.chdir(temporary)
    root = Path(temporary).resolve()
    profile = root / 'target/debug'
    profile.mkdir(parents=True)
    windows = os.name == 'nt'
    binary = profile / ('zg.exe' if windows else 'zg')
    core = 'llama.dll' if windows else 'libllama.so.0'
    plugin = 'ggml-cuda.dll' if windows else 'libggml-cuda.so'
    sdk = root / 'sdk'
    (sdk / 'lib/cuda12').mkdir(parents=True)
    for name in (core, plugin, 'cuda12/' + plugin):
        (sdk / 'lib' / name).write_bytes(name.encode())
    manifest = {'files': {p.relative_to(sdk).as_posix(): runtime.digest(p)
                          for p in sdk.rglob('*') if p.is_file()}}
    (sdk / 'manifest.json').write_text(json.dumps(manifest))
    # Cache leftovers must not make a broken build.rs appear to work.
    for directory in (profile, profile / 'deps', profile / 'examples', profile / 'cuda12'):
        directory.mkdir(exist_ok=True)
        (directory / core).write_bytes(b'stale helper copy')
        (directory / 'keep.txt').write_bytes(b'unrelated')
    runtime.reset_cargo_cli(Path('target/debug'))
    assert not list(profile.rglob(core))
    assert all(p.read_bytes() == b'unrelated' for p in profile.rglob('keep.txt'))
    try:
        runtime.reset_cargo_cli(sdk / 'lib')
    except RuntimeError:
        pass
    else:
        raise AssertionError('reset outside target/debug was accepted')
    binary.write_bytes(b'original Cargo executable')
    records = [{'reason': 'compiler-artifact', 'target': {'name': 'zg', 'kind': ['bin']},
                'profile': {'test': False}, 'features': ['cuda'], 'executable': str(binary)}]
    messages = root / 'messages.json'
    messages.write_text(json.dumps(records[0]) + '\n')
    sdk_root = ('C:/zg-ci-runtimes/' if windows else '/work/rust/target/llama-runtimes/') + 'a' * 64 + '/bundle'
    runtime.checked_cargo_sdk_root(sdk_root, windows)
    for invalid in ('/tmp/sdk', 'C:/Windows', sdk_root + '/../escape'):
        try:
            runtime.checked_cargo_sdk_root(invalid, windows)
        except RuntimeError:
            pass
        else:
            raise AssertionError('unsafe SDK restore path accepted')
    os.environ['SOURCE_SHA'] = 'test-source'
    archive = root / 'cli.tgz'
    with patch.object(runtime, 'backend_directory', return_value=sdk / 'lib'), \
         patch.object(runtime, 'checked_cargo_sdk_root', return_value=sdk_root):
        runtime.archive_cargo_cli(messages, archive)
    with tarfile.open(archive) as package:
        # This tiny fixture also runs with macOS's Python 3.9. Production CI
        # extracts the real artifact with Python 3.12's data filter.
        for member in package:
            destination = (root / 'received' / member.name).resolve()
            assert member.isfile() and destination.is_relative_to(root / 'received')
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(package.extractfile(member).read())
    bundle = root / 'received/cargo-cli'
    receipt = runtime.verify_cargo_cli(bundle)
    assert (bundle / receipt['binary']).read_bytes() == binary.read_bytes()
    assert not (bundle / 'profile' / core).exists(), 'snapshot silently repaired a missing Cargo library'
    assert (bundle / 'sdk/lib' / core).is_file()
    assert not (bundle / 'sdk/lib/cuda12' / plugin).exists(), 'unused fallback bloats GPU transfer'
    # A fresh GPU checkout has neither nested mountpoint. Prepare empty host
    # directories before mounting /work read-only, never repair the payload.
    workspace = root / 'gpu workspace'
    linux_sdk = '/work/rust/target/llama-runtimes/' + 'a' * 64 + '/bundle'
    linux_receipt = {**receipt, 'platform': 'linux', 'sdk_root': linux_sdk}
    mountpoints = [workspace / 'rust/target/debug',
                   workspace / PurePosixPath(linux_sdk).relative_to('/work')]
    assert not any(path.exists() for path in mountpoints)
    before = {p.relative_to(bundle): runtime.digest(p) for p in bundle.rglob('*') if p.is_file()}
    inherited_environment = dict(os.environ)
    mounts = runtime.cargo_cli_linux_mounts(workspace, bundle, linux_receipt)
    assert mounts == [f'{workspace}:/work:ro',
                      str(bundle / 'profile') + ':/work/rust/target/debug:ro',
                      str(bundle / 'sdk') + f':{linux_sdk}:ro']
    assert all(path.is_dir() and not list(path.iterdir()) for path in mountpoints)
    assert runtime.cargo_cli_linux_mounts(workspace, bundle, linux_receipt) == mounts
    assert before == {p.relative_to(bundle): runtime.digest(p) for p in bundle.rglob('*') if p.is_file()}
    assert dict(os.environ) == inherited_environment, 'mount preparation changed library lookup'
    runtime.verify_cargo_cli(bundle)
    for invalid in ({**linux_receipt, 'platform': 'windows'},
                    {**linux_receipt, 'sdk_root': '/tmp/untrusted-sdk'}):
        rejected_workspace = root / 'rejected workspace'
        try:
            runtime.cargo_cli_linux_mounts(rejected_workspace, bundle, invalid)
        except RuntimeError:
            pass
        else:
            raise AssertionError('invalid Linux mount receipt accepted')
        assert not rejected_workspace.exists(), 'invalid receipt created mountpoints'
    (bundle / receipt['binary']).write_bytes(b'changed')
    try:
        runtime.verify_cargo_cli(bundle)
    except RuntimeError:
        pass
    else:
        raise AssertionError('modified CLI snapshot accepted')
    (bundle / receipt['binary']).write_bytes(binary.read_bytes())
    os.environ['SOURCE_SHA'] = 'another-source'
    try:
        runtime.verify_cargo_cli(bundle)
    except RuntimeError:
        pass
    else:
        raise AssertionError('snapshot from another commit accepted')
    environment = dict(os.environ)
    overrides = ('LD_LIBRARY_PATH', 'LD_PRELOAD', 'DYLD_LIBRARY_PATH',
                 'DYLD_FALLBACK_LIBRARY_PATH', 'DYLD_INSERT_LIBRARIES',
                 'ORT_DYLIB_PATH', 'ZVEC_GREP_NATIVE_LIB_DIR', 'GGML_BACKEND_PATH')
    environment.update({key: str(sdk / 'lib') for key in overrides})
    vendor = root / 'cuda/bin'
    vendor.mkdir(parents=True)
    environment['PATH'] = os.pathsep.join([str(profile), str(sdk / 'lib'), str(vendor), os.defpath])
    cleaned = e2e.cargo_environment(binary, environment)
    assert not set(overrides).intersection(cleaned)
    assert str(profile) not in cleaned['PATH'] and str(sdk / 'lib') not in cleaned['PATH']
    assert str(vendor) in cleaned['PATH'], 'host CUDA lookup must survive'
    # Check the actual child environment, not just the dictionary returned above.
    result = subprocess.run([sys.executable, '-c', 'import json, os; print(json.dumps(dict(os.environ)))'],
                            env=cleaned, capture_output=True, text=True, check=True)
    child = json.loads(result.stdout)
    assert not set(overrides).intersection(child)
assert Path.cwd() == previous_directory
assert not root.exists(), 'temporary fixture was not removed'
print('Cargo CLI transport and clean-environment regression checks passed')
";

#[test]
fn cargo_cli_regression_preserves_raw_output_and_removes_lookup_overrides() {
    let python = std::env::var_os("PYTHON")
        .unwrap_or_else(|| if cfg!(windows) { "python" } else { "python3" }.into());
    let result = std::process::Command::new(python)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg("-c")
        .arg(CARGO_CLI_REGRESSION_CHECK)
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/cuda"))
        .output()
        .expect("Python 3 for Cargo CLI regression checks");
    assert!(
        result.status.success(),
        "Cargo CLI regression checks failed:\n{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

const LINUX: &str = "x86_64-unknown-linux-gnu";
const WINDOWS: &str = "x86_64-pc-windows-msvc";

fn save(root: &Path, manifest: &Value) {
    fs::write(
        root.join("manifest.json"),
        serde_json::to_vec(manifest).expect("JSON"),
    )
    .expect("write manifest");
}

fn fixture(target: &str) -> (tempfile::TempDir, Value) {
    let sdk = tempfile::tempdir().expect("SDK directory");
    let mut manifest = json!({"schema": 1, "target": target, "linkage": "dynamic",
        "link_libraries": ["dylib=llama", "dylib=ggml", "static=ggml-cpu"], "files": {}});
    let names = if target == WINDOWS {
        // Match the pinned Windows SDK: the common-base CMake target is STATIC,
        // but its MSVC .lib is recorded with the same dylib= link directive.
        manifest["link_libraries"]
            .as_array_mut()
            .expect("link libraries")
            .push(json!("dylib=llama-common-base"));
        vec![
            "llama.dll",
            "llama.lib",
            "ggml.dll",
            "ggml.lib",
            "llama-common-base.lib",
            "ggml-cpu.lib",
            "ggml-cpu-x64.dll",
            "ggml-cuda.dll",
            "cuda12/ggml-cuda.dll",
        ]
    } else {
        vec![
            "libllama.so",
            "libllama.so.0",
            "libllama.so.0.0.1",
            "libggml.so",
            "libggml.so.0",
            "libggml-cpu.a",
            "libggml-cpu.so",
            "libggml-cpu-x64.so",
            "libggml-cuda.so",
            "cuda12/libggml-cuda.so",
        ]
    };
    for name in names {
        let relative = format!("lib/{name}");
        let path = sdk.path().join(&relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        fs::write(&path, name).expect("runtime fixture");
        let mut digest = String::with_capacity(64);
        for byte in Sha256::digest(name.as_bytes()) {
            write!(digest, "{byte:02x}").expect("write digest");
        }
        manifest["files"][relative] = json!(digest);
    }
    save(sdk.path(), &manifest);
    (sdk, manifest)
}

#[test]
fn linux_sonames_are_staged_for_binaries_tests_and_examples_in_each_profile() {
    for profile in [
        "debug",
        "release",
        "x86_64-unknown-linux-gnu/custom-profile",
    ] {
        let (sdk, _) = fixture(LINUX);
        let target = tempfile::tempdir().expect("target");
        let profile = target.path().join(profile);
        let files = llama_runtime::stage(
            sdk.path(),
            &sdk.path().join("manifest.json"),
            LINUX,
            &profile.join("build/zg-hash/out"),
        )
        .expect("stage");
        assert_eq!(files.len(), 15);
        for directory in [
            profile.clone(),
            profile.join("deps"),
            profile.join("examples"),
        ] {
            for name in [
                "libllama.so",
                "libllama.so.0",
                "libllama.so.0.0.1",
                "libggml.so",
                "libggml.so.0",
            ] {
                assert_eq!(
                    fs::read(directory.join(name)).expect("staged library"),
                    name.as_bytes()
                );
            }
            assert!(!directory.join("cuda12").exists());
            assert!(!directory.join("libggml-cuda.so").exists());
            assert!(!directory.join("libggml-cpu.so").exists());
        }
    }
}

#[test]
fn windows_stages_dlls_not_import_libraries_or_plugins() {
    let (sdk, _) = fixture(WINDOWS);
    let target = tempfile::tempdir().expect("target");
    let files = llama_runtime::stage(
        sdk.path(),
        &sdk.path().join("manifest.json"),
        WINDOWS,
        &target.path().join("release/build/zg/out"),
    )
    .expect("stage");
    assert_eq!(files.len(), 6);
    assert!(files.iter().all(|p| {
        ["llama.dll", "ggml.dll"]
            .contains(&p.file_name().expect("filename").to_str().expect("UTF-8"))
    }));
}

#[test]
fn missing_windows_runtime_is_not_hidden_by_its_import_library() {
    let (sdk, mut manifest) = fixture(WINDOWS);
    manifest["files"]
        .as_object_mut()
        .expect("payloads")
        .remove("lib/llama.dll");
    fs::remove_file(sdk.path().join("lib/llama.dll")).expect("remove DLL");
    save(sdk.path(), &manifest);
    let target = tempfile::tempdir().expect("target");
    let error = llama_runtime::stage(
        sdk.path(),
        &sdk.path().join("manifest.json"),
        WINDOWS,
        &target.path().join("release/build/zg/out"),
    )
    .expect_err("the import .lib does not replace its required DLL");
    assert!(
        error
            .to_string()
            .contains("missing shared runtime for llama.cpp link library: llama")
    );
}

#[test]
fn unchanged_build_preserves_copies_and_rebuild_repairs_missing_or_modified_files() {
    let (sdk, _) = fixture(LINUX);
    let target = tempfile::tempdir().expect("target");
    let profile = target.path().join("release");
    let out = profile.join("build/zg/out");
    let stage = || llama_runtime::stage(sdk.path(), &sdk.path().join("manifest.json"), LINUX, &out);
    let files = stage().expect("stage");
    assert_eq!(
        fs::metadata(profile.join("libllama.so.0"))
            .expect("staged file")
            .modified()
            .expect("mtime"),
        fs::metadata(sdk.path().join("lib/libllama.so.0"))
            .expect("SDK")
            .modified()
            .expect("mtime"),
    );
    let before = fs::metadata(&files[0])
        .expect("metadata")
        .modified()
        .expect("mtime");
    stage().expect("incremental stage");
    assert_eq!(
        before,
        fs::metadata(&files[0])
            .expect("metadata")
            .modified()
            .expect("mtime")
    );
    fs::write(profile.join("libllama.so.0"), "consumer modification").expect("modify copy");
    fs::remove_file(profile.join("deps/libggml.so.0")).expect("remove staged file");
    fs::write(profile.join("unrelated.dll"), "keep").expect("unrelated file");
    stage().expect("repair");
    assert_eq!(
        fs::read(profile.join("libllama.so.0")).expect("repaired"),
        b"libllama.so.0"
    );
    assert_eq!(
        fs::read(profile.join("deps/libggml.so.0")).expect("restored"),
        b"libggml.so.0"
    );
    assert_eq!(
        fs::read(sdk.path().join("lib/libllama.so.0")).expect("SDK unchanged"),
        b"libllama.so.0"
    );
    assert_eq!(
        fs::read(profile.join("unrelated.dll")).expect("preserved"),
        b"keep"
    );
}

#[test]
fn wrong_metadata_or_modified_sdk_fails_instead_of_using_another_cached_sdk() {
    let (sdk, manifest) = fixture(LINUX);
    let target = tempfile::tempdir().expect("target");
    let out = target.path().join("release/build/zg/out");
    for (field, value) in [
        ("schema", json!(2)),
        ("target", json!(WINDOWS)),
        ("linkage", json!("unknown")),
    ] {
        let mut changed = manifest.clone();
        changed[field] = value;
        save(sdk.path(), &changed);
        assert!(
            llama_runtime::stage(sdk.path(), &sdk.path().join("manifest.json"), LINUX, &out)
                .is_err()
        );
    }
    save(sdk.path(), &manifest);
    fs::write(sdk.path().join("lib/libllama.so.0"), "corrupted SDK").expect("corrupt fixture");
    assert!(
        llama_runtime::stage(sdk.path(), &sdk.path().join("manifest.json"), LINUX, &out).is_err()
    );
    assert!(
        llama_runtime::stage(
            target.path(),
            &sdk.path().join("manifest.json"),
            LINUX,
            &out
        )
        .is_err()
    );
}

#[test]
fn static_sdk_has_no_shared_library_deployment() {
    let (sdk, mut manifest) = fixture(LINUX);
    manifest["linkage"] = json!("static");
    save(sdk.path(), &manifest);
    let target = tempfile::tempdir().expect("target");
    let profile = target.path().join("release");
    assert!(
        llama_runtime::stage(
            sdk.path(),
            &sdk.path().join("manifest.json"),
            LINUX,
            &profile.join("build/zg/out")
        )
        .expect("static SDK")
        .is_empty()
    );
    assert!(!profile.exists());
}

#[test]
fn deployment_failure_is_retryable_without_touching_sdk() {
    let (sdk, _) = fixture(LINUX);
    let target = tempfile::tempdir().expect("target");
    let profile = target.path().join("release");
    fs::create_dir(&profile).expect("profile");
    fs::write(profile.join("examples"), "not a directory").expect("block directory");
    let stage = || {
        llama_runtime::stage(
            sdk.path(),
            &sdk.path().join("manifest.json"),
            LINUX,
            &profile.join("build/zg/out"),
        )
    };
    assert!(stage().is_err());
    fs::remove_file(profile.join("examples")).expect("unblock fixture");
    stage().expect("retry succeeds");
    assert_eq!(
        fs::read(sdk.path().join("lib/libggml.so.0")).expect("SDK unchanged"),
        b"libggml.so.0"
    );
}

#[cfg(unix)]
#[test]
fn replaces_destination_symlinks_without_changing_their_targets_or_hardlinking_sdk() {
    use std::os::unix::fs::MetadataExt;
    let (sdk, _) = fixture(LINUX);
    let target = tempfile::tempdir().expect("target");
    let profile = target.path().join("release");
    fs::create_dir(&profile).expect("profile");
    let unrelated = target.path().join("unrelated");
    fs::write(&unrelated, "keep").expect("unrelated file");
    std::os::unix::fs::symlink(&unrelated, profile.join("libllama.so.0")).expect("symlink");
    llama_runtime::stage(
        sdk.path(),
        &sdk.path().join("manifest.json"),
        LINUX,
        &profile.join("build/zg/out"),
    )
    .expect("stage");
    assert_eq!(fs::read(&unrelated).expect("preserved"), b"keep");
    assert!(
        !fs::symlink_metadata(profile.join("libllama.so.0"))
            .expect("copy")
            .is_symlink()
    );
    assert_ne!(
        fs::metadata(profile.join("libllama.so.0"))
            .expect("copy")
            .ino(),
        fs::metadata(sdk.path().join("lib/libllama.so.0"))
            .expect("SDK")
            .ino()
    );
}

#[cfg(unix)]
fn source_fixture(directory: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("source output");
    fs::create_dir(root.path().join("backends")).expect("backends");
    let libraries = root.path().join(directory);
    fs::create_dir(&libraries).expect("libraries");
    for library in ["llama", "ggml", "ggml-base"] {
        let name = format!("lib{library}.so");
        fs::write(libraries.join(format!("{name}.0.0.1")), library).expect("versioned library");
        std::os::unix::fs::symlink(format!("{name}.0.0.1"), libraries.join(format!("{name}.0")))
            .expect("SONAME alias");
        std::os::unix::fs::symlink(format!("{name}.0"), libraries.join(&name))
            .expect("linker alias");
    }
    root
}

#[cfg(unix)]
#[test]
fn source_soname_chains_are_copied_and_dangling_output_links_are_repaired() {
    for layout in ["lib", "lib64", "bin"] {
        let source = source_fixture(layout);
        let target = tempfile::tempdir().expect("target");
        let profile = target.path().join("triple/custom-profile");
        fs::create_dir_all(&profile).expect("profile");
        // Reproduce the wrapper's linker alias without its versioned target.
        std::os::unix::fs::symlink("libllama.so.0", profile.join("libllama.so"))
            .expect("dangling link");
        for name in [
            "libggml-cuda.so",
            "libggml-cpu.so",
            "libunrelated.so",
            "libllama.a",
        ] {
            fs::write(source.path().join(layout).join(name), "not a core").expect("extra file");
        }
        let stage = || {
            llama_runtime::stage_source(
                &source.path().join("backends"),
                LINUX,
                &profile.join("build/zg/out"),
            )
        };
        stage().expect("stage source aliases");
        for directory in [&profile, &profile.join("deps"), &profile.join("examples")] {
            for library in ["llama", "ggml", "ggml-base"] {
                for suffix in ["", ".0", ".0.0.1"] {
                    let path = directory.join(format!("lib{library}.so{suffix}"));
                    assert!(fs::symlink_metadata(&path).expect("copy").is_file());
                    assert_eq!(fs::read(&path).expect("payload"), library.as_bytes());
                }
            }
            assert_eq!(
                fs::read_dir(directory)
                    .expect("files")
                    .filter_map(Result::ok)
                    .filter(|entry| entry.path().is_file())
                    .count(),
                9
            );
        }
        let library = profile.join("libllama.so.0");
        let before = fs::metadata(&library)
            .expect("library")
            .modified()
            .expect("mtime");
        stage().expect("repeat deployment");
        assert_eq!(
            fs::metadata(&library)
                .expect("library")
                .modified()
                .expect("mtime"),
            before
        );
        fs::remove_file(&library).expect("remove deployed file");
        stage().expect("repair missing deployment");
        fs::write(&library, "modified copy").expect("edit deployed copy");
        assert_eq!(
            fs::read(source.path().join(layout).join("libllama.so.0")).expect("source"),
            b"llama"
        );
    }
}

#[cfg(unix)]
#[test]
fn invalid_source_runtimes_fail_before_deploying_any_library() {
    for fault in [
        "missing-core",
        "broken-link",
        "outside-link",
        "conflicting-copy",
        "wrong-root",
    ] {
        let source = source_fixture("lib64");
        let target = tempfile::tempdir().expect("target");
        let profile = target.path().join("release");
        let libraries = source.path().join("lib64");
        match fault {
            "missing-core" => {
                for suffix in ["", ".0", ".0.0.1"] {
                    fs::remove_file(libraries.join(format!("libggml.so{suffix}")))
                        .expect("remove core");
                }
            }
            "broken-link" => {
                fs::remove_file(libraries.join("libllama.so.0.0.1")).expect("break chain");
            }
            "outside-link" => {
                let external = target.path().join("external.so");
                fs::write(&external, "outside this dependency").expect("external library");
                fs::remove_file(libraries.join("libllama.so.0.0.1")).expect("replace target");
                std::os::unix::fs::symlink(external, libraries.join("libllama.so.0.0.1"))
                    .expect("external link");
            }
            "conflicting-copy" => {
                fs::create_dir(source.path().join("lib")).expect("other libdir");
                fs::write(source.path().join("lib/libllama.so.0"), "different build")
                    .expect("conflict");
            }
            "wrong-root" => {}
            _ => unreachable!(),
        }
        let backends = source.path().join(if fault == "wrong-root" {
            "lib64"
        } else {
            "backends"
        });
        assert!(
            llama_runtime::stage_source(&backends, LINUX, &profile.join("build/zg/out")).is_err(),
            "{fault}"
        );
        assert!(
            !profile.exists(),
            "{fault}: invalid input partially deployed"
        );
    }
}

#[test]
fn source_windows_copies_only_the_three_core_dlls() {
    let source = tempfile::tempdir().expect("source");
    fs::create_dir(source.path().join("backends")).expect("backends");
    let bin = source.path().join("bin");
    fs::create_dir(&bin).expect("bin");
    for name in [
        "llama.dll",
        "ggml.dll",
        "ggml-base.dll",
        "llama.lib",
        "ggml-cuda.dll",
        "ggml-cpu.dll",
    ] {
        fs::write(bin.join(name), name).expect("DLL fixture");
    }
    let target = tempfile::tempdir().expect("target");
    let profile = target.path().join("debug");
    llama_runtime::stage_source(
        &source.path().join("backends"),
        WINDOWS,
        &profile.join("build/zg/out"),
    )
    .expect("deploy source DLLs");
    for directory in [&profile, &profile.join("deps"), &profile.join("examples")] {
        let mut names: Vec<_> = fs::read_dir(directory)
            .expect("files")
            .map(|entry| entry.expect("file").file_name())
            .collect();
        names.retain(|name| directory.join(name).is_file());
        names.sort();
        assert_eq!(names, ["ggml-base.dll", "ggml.dll", "llama.dll"]);
    }
}

#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), ignore = "ELF startup requires Linux")]
#[test]
fn source_elf_starts_outside_cargo_without_library_path_overrides() {
    use std::process::Command;
    // Three tiny C libraries exercise real ELF SONAME resolution, not a llama
    // or CUDA compilation. The same deployment helper handles the real build.
    let source = source_fixture("lib64");
    let libraries = source.path().join("lib64");
    for (library, symbol) in [("llama", "llama"), ("ggml", "ggml"), ("ggml-base", "base")] {
        let input = source.path().join(format!("{library}.c"));
        fs::write(
            &input,
            format!("int {symbol}_fixture(void) {{ return 14; }}\n"),
        )
        .expect("C fixture");
        let output = Command::new("cc")
            .args(["-shared", "-fPIC"])
            .arg(format!("-Wl,-soname,lib{library}.so.0"))
            .arg(&input)
            .arg("-o")
            .arg(libraries.join(format!("lib{library}.so.0.0.1")))
            .output()
            .expect("C compiler");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let target = tempfile::tempdir().expect("target");
    let profile = target.path().join("release");
    fs::create_dir(&profile).expect("profile");
    let input = target.path().join("main.c");
    fs::write(&input, "int llama_fixture(void); int ggml_fixture(void); int base_fixture(void);\nint main(void) { return llama_fixture() + ggml_fixture() + base_fixture() != 42; }\n").expect("C program");
    let binary = profile.join("runtime-probe");
    let output = Command::new("cc")
        .arg(&input)
        .arg("-L")
        .arg(&libraries)
        .args([
            "-Wl,-rpath,$ORIGIN",
            "-l",
            "llama",
            "-l",
            "ggml",
            "-l",
            "ggml-base",
            "-o",
        ])
        .arg(&binary)
        .output()
        .expect("link probe");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = || {
        Command::new(&binary)
            .current_dir(target.path())
            .env_remove("LD_LIBRARY_PATH")
            .env_remove("LD_PRELOAD")
            .output()
            .expect("run ELF probe")
    };
    let missing = run();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("libllama.so.0"));
    // The old wrapper leaves only this dangling unversioned alias behind.
    std::os::unix::fs::symlink("libllama.so.0", profile.join("libllama.so"))
        .expect("dangling alias");
    llama_runtime::stage_source(
        &source.path().join("backends"),
        LINUX,
        &profile.join("build/zg/out"),
    )
    .expect("deploy actual ELF libraries");
    // Prove the executable cannot be rescued by the original source location.
    source.close().expect("remove source fixture");
    let deployed = run();
    assert!(
        deployed.status.success(),
        "{}",
        String::from_utf8_lossy(&deployed.stderr)
    );
}
