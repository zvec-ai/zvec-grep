#!/usr/bin/env python3
"""Build pinned PDFium sources into a cached static SDK, then emit Cargo links."""

import argparse
import ast
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import time

MANIFEST_PATH = Path(__file__).with_name("pdfium-build.json")
MARKER = "pdfium-static.json"


def digest(path):
    sha = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            sha.update(block)
    return sha.hexdigest()


def run(arguments, cwd, env=None, capture=False):
    print("PDFium: " + " ".join(map(str, arguments)), file=sys.stderr, flush=True)
    result = subprocess.run(list(map(str, arguments)), cwd=cwd, env=env, check=True,
                            stdout=subprocess.PIPE if capture else sys.stderr, text=True)
    return result.stdout.strip() if capture else None


@contextmanager
def build_lock(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a+b") as stream:
        if os.name == "nt":
            import msvcrt
            stream.seek(0)
            stream.write(b"0")
            stream.flush()
            while True:
                try:
                    stream.seek(0)
                    msvcrt.locking(stream.fileno(), msvcrt.LK_NBLCK, 1)
                    break
                except OSError:
                    time.sleep(1)
        else:
            import fcntl
            fcntl.flock(stream.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            if os.name == "nt":
                stream.seek(0)
                msvcrt.locking(stream.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(stream.fileno(), fcntl.LOCK_UN)


def identity(manifest, target):
    return {"version": manifest["version"], "revision": manifest["revision"],
            "target": target, "compiler": manifest["targets"][target].get("compiler", "clang"),
            "recipe": digest(Path(__file__)),
            "runtime_notices": digest(Path(__file__).with_name("licenses") / "gcc-runtime.txt"),
            "manifest": hashlib.sha256(json.dumps(manifest, sort_keys=True).encode()).hexdigest(),
            "macos_target": os.environ.get("MACOSX_DEPLOYMENT_TARGET", "")}


def verified_sdk(sdk, expected):
    try:
        state = json.loads((sdk / MARKER).read_text())
        if state["identity"] != expected or not state["files"]:
            return False
        required = {"licenses/pdfium-ThirdPartyNotices.txt",
                    "lib/pdfium.lib" if "windows" in expected["target"] else "lib/libpdfium.a"}
        if "linux" in expected["target"]:
            required.update(["lib/libstdc++.a"] if expected["compiler"] == "gcc" else
                            ["lib/libc++.a", "lib/libc++abi.a"])
        if set(state["files"]) != required:
            return False
        for name, sha in state["files"].items():
            path = sdk / name
            if path.is_symlink() or digest(path) != sha:
                return False
            if name.startswith("lib/"):
                with path.open("rb") as stream:
                    if stream.read(8) != b"!<arch>\n":
                        return False
        return True
    except (OSError, ValueError, KeyError):
        return False


def checkout(path, repository, revision):
    if not (path / ".git").is_dir():
        path.mkdir(parents=True, exist_ok=True)
        run(["git", "init", path], path.parent)
        run(["git", "remote", "add", "origin", repository], path)
    current = subprocess.run(["git", "rev-parse", "HEAD"], cwd=path,
                             capture_output=True, text=True)
    if current.returncode == 0 and current.stdout.strip() == revision:
        return
    run(["git", "fetch", "--depth=1", "origin", revision], path)
    run(["git", "checkout", "--detach", revision], path)


def library_deps(text):
    """Exclude unused build services/toolchains; retain all library dependencies."""
    unused = {"third_party/rust-toolchain", "third_party/siso/cipd", "buildtools/reclient", "tools/resultdb",
              "third_party/cpython3/host", "third_party/cpython3/linux-amd64",
              "tools/skia_goldctl/linux", "tools/skia_goldctl/mac_amd64",
              "tools/skia_goldctl/mac_arm64", "tools/skia_goldctl/win"}
    lines = text.splitlines(keepends=True)
    offsets = [0]
    for line in lines:
        offsets.append(offsets[-1] + len(line))
    edits = []
    for node in ast.walk(ast.parse(text)):
        if isinstance(node, ast.Dict):
            for key, value in zip(node.keys, node.values):
                if isinstance(key, ast.Constant) and key.value in unused:
                    edits.append((offsets[value.lineno - 1] + value.col_offset,
                                  offsets[value.end_lineno - 1] + value.end_col_offset))
    for start, end in sorted(edits, reverse=True):
        text = text[:start] + "None" + text[end:]
    return text


def source_tree(root, manifest, target):
    depot = root / "depot_tools"
    checkout(depot, **manifest["depot_tools"])
    source = root / "pdfium"
    checkout(source, manifest["repository"], manifest["revision"])
    pristine_deps = run(["git", "show", manifest["revision"] + ":DEPS"], source, capture=True)
    # gclient custom_deps does not override GCS/CIPD entries in this revision.
    # Patch the build-only DEPS selection, never PDFium's rendering sources.
    (source / "DEPS").write_text(library_deps(pristine_deps) + "\n")
    python = sys.executable
    if sys.version_info < (3, 11):
        python = next((shutil.which(name) for name in ["python3.14", "python3.13", "python3.12", "python3.11"]
                       if shutil.which(name)), None)
        if not python:
            raise RuntimeError("Building PDFium from source requires Python 3.11 or newer (set PYTHON)")
    env = dict(os.environ, PATH=str(Path(python).parent) + os.pathsep + str(depot) + os.pathsep + os.environ["PATH"],
               DEPOT_TOOLS_UPDATE="0", DEPOT_TOOLS_WIN_TOOLCHAIN="0")
    spec = manifest["targets"][target]
    config = {"name": "pdfium", "url": manifest["repository"], "managed": False,
              "custom_vars": {"checkout_configuration": "minimal"}}
    if spec.get("compiler") == "gcc":
        # Chromium only distributes an x64 Linux compiler. Native ARM64 uses
        # its system GCC; do not download an executable for the wrong CPU.
        config["custom_vars"]["non_git_source"] = False
    (root / ".gclient").write_text(
        f"solutions = [{config!r}]\ntarget_os = [{spec['os']!r}]\n")
    synced = root / ".synced"
    stamp = manifest["revision"] + ":" + json.dumps([spec, config], sort_keys=True) + ":" + digest(Path(__file__))
    if not synced.is_file() or synced.read_text() != stamp:
        # Parallel GCS downloads otherwise race while bootstrapping depot_tools'
        # shared CIPD/vpython directory on a clean machine. Initialize once.
        run([python, depot / "gsutil.py", "version"], root, env)
        # The primary checkout was already verified above. Keep its build-only
        # DEPS patch while gclient checks out the pinned transitive dependencies.
        run([python, depot / "gclient.py", "sync", "--revision", "pdfium@unmanaged",
             "--no-history", "--shallow", "--nohooks"], root, env)
        actual = run(["git", "rev-parse", "HEAD"], root / "pdfium", capture=True)
        if actual != manifest["revision"]:
            raise RuntimeError("PDFium checkout does not match the pinned revision")
        # A library build does not need test fonts, dsymutil or the other
        # executable/test hooks. DEPS already pins the compiler, GN and Ninja.
        run([python, "build/util/lastchange.py", "-o", "build/util/LASTCHANGE"],
            root / "pdfium", env)
        synced.write_text(stamp)
    return root / "pdfium", env, python


def gn_arguments(spec):
    args = {"is_debug": False, "is_component_build": False, "pdf_is_standalone": True,
            "pdf_is_complete_lib": True, "pdf_use_partition_alloc": False,
            "pdf_enable_v8": False, "pdf_enable_xfa": False, "symbol_level": 0,
            "treat_warnings_as_errors": False, "clang_use_chrome_plugins": False,
            "use_siso": False, "use_remoteexec": False,
            "target_os": spec["os"], "target_cpu": spec["cpu"],
            "use_custom_libcxx": spec["os"] == "linux" and spec.get("compiler") != "gcc"}
    if spec.get("compiler") == "gcc":
        args.update(is_clang=False, use_lld=False)
    if spec["os"] == "linux":
        # Build against the release builder's glibc 2.28 headers, not Chromium's
        # newer Debian sysroot. Bundle Chromium's matching C++ static runtimes.
        args["use_sysroot"] = False
        # HarfBuzz's optional GLib helpers are unnecessary for PDF rendering.
        args["use_glib"] = False
    if spec["os"] == "mac":
        args["use_system_xcode"] = True
        if os.environ.get("MACOSX_DEPLOYMENT_TARGET"):
            args["mac_deployment_target"] = os.environ["MACOSX_DEPLOYMENT_TARGET"]
    return args


def windows_sdk(env, cpu):
    root = Path(env.get("WINDOWSSDKDIR") or env.get("WindowsSdkDir") or
                str(Path(env.get("ProgramFiles(x86)", r"C:\Program Files (x86)")) / "Windows Kits/10"))
    preferred = env.get("WindowsSDKVersion", "").rstrip("\\/")
    versions = sorted((p.name for p in (root / "Include").glob("10.*") if p.is_dir()),
                      key=lambda value: tuple(int(n) for n in value.split(".")), reverse=True)
    for version in dict.fromkeys([preferred, *versions]):
        if not re.fullmatch(r"10\.\d+\.\d+\.\d+", version):
            continue
        required = [root / "Include" / version / "um/Windows.h",
                    root / "Include" / version / "ucrt/stdio.h",
                    root / "Lib" / version / "um" / cpu / "kernel32.lib",
                    root / "Lib" / version / "ucrt" / cpu / "ucrt.lib"]
        if all(path.is_file() for path in required):
            return root, version
    raise RuntimeError(f"No complete Windows SDK for {cpu} under {root}; install the Windows C++ SDK")


def platform_toolchain(source, env, host):
    if host == "mac":
        config = source / "build/config/mac/mac_sdk.gni"
        pristine = run(["git", "show", "HEAD:config/mac/mac_sdk.gni"], source / "build", capture=True)
        developer = Path(run(["xcode-select", "-print-path"], source, env, capture=True))
        if (developer / "SDKs").is_dir() and (developer / "usr/bin").is_dir():
            # Chromium expects full Xcode. The static C++ library only needs
            # the SDK and command-line tools; discover those without xcodebuild.
            sdk = run(["xcrun", "--sdk", "macosx", "--show-sdk-path"], source, env, capture=True)
            version = run(["xcrun", "--sdk", "macosx", "--show-sdk-version"], source, env, capture=True)
            build = run(["xcrun", "--sdk", "macosx", "--show-sdk-build-version"], source, env, capture=True)
            import plistlib
            package = plistlib.loads(subprocess.check_output(
                ["pkgutil", "--pkg-info-plist", "com.apple.pkg.CLTools_Executables"], env=env))
            parts = package["pkg-version"].split(".")
            tools_version = f"{int(parts[0]):02d}{parts[1]}{parts[2]}"
            sdk_link = source / "out/zg-static/sdk/CommandLineTools.sdk"
            sdk_link.parent.mkdir(parents=True, exist_ok=True)
            if sdk_link.is_symlink():
                sdk_link.unlink()
            sdk_link.symlink_to(sdk, target_is_directory=True)
            values = {"mac_sdk_path": "//out/zg-static/sdk/CommandLineTools.sdk", "mac_sdk_version": version,
                      "mac_sdk_build_version": build, "mac_bin_path": str(developer / "usr/bin") + "/",
                      "xcode_version": tools_version, "xcode_version_int": int(tools_version),
                      "xcode_build": "CLT_" + package["pkg-version"].replace(".", "_")}
            begin = pristine.index("# The path to the hermetic install of Xcode.")
            end = pristine.index("_sdk_root =", begin)
            pristine = pristine[:begin] + "".join(
                f"{key} = {json.dumps(value)}\n" for key, value in values.items()) + "\n" + pristine[end:]
        config.write_text(pristine)
    if host == "win":
        # Chromium pins an SDK that is not installed on standard VS runners.
        # Select a complete installed SDK and keep both build helpers in sync.
        cpu = "arm64" if "arm64" in env.get("VSCMD_ARG_TGT_ARCH", "").lower() else "x64"
        sdk_root, sdk_version = windows_sdk(env, cpu)
        env["WINDOWSSDKDIR"] = str(sdk_root)
        if env.get("VSINSTALLDIR"):
            env["GYP_MSVS_OVERRIDE_PATH"] = env["VSINSTALLDIR"].rstrip("\\/")
        for relative in ["build/vs_toolchain.py", "build/toolchain/win/setup_toolchain.py"]:
            config = source / relative
            text, count = re.subn(r"(?m)^SDK_VERSION = ['\"][^'\"]+['\"]", "SDK_VERSION = " + repr(sdk_version), config.read_text())
            if count != 1:
                raise RuntimeError(f"Cannot select installed Windows SDK in {config}")
            config.write_text(text)
        # Chromium defaults to /MT for static libraries. Match Rust's /MD CRT
        # without changing zg, zvec or ORT's existing runtime configuration.
        config = source / "build/config/win/BUILD.gn"
        text = config.read_text()
        text = text.replace('configs = [ ":static_crt" ]', 'configs = [ ":dynamic_crt" ]')
        config.write_text(text)
    if host == "linux" and platform.machine().lower() in {"aarch64", "arm64"}:
        config = source / "build/toolchain/linux/BUILD.gn"
        # GN's ARM64 GCC template assumes a cross compiler; this is a native
        # build inside the same manylinux image used to compile zg.
        text = config.read_text().replace('toolprefix = "aarch64-linux-gnu-"', 'toolprefix = ""')
        config.write_text(text)


def portable_archive(source, destination, ar, env):
    """Thin archives point into the build tree; cache only self-contained ones."""
    destination.parent.mkdir(parents=True, exist_ok=True)
    with source.open("rb") as stream:
        magic = stream.read(8)
    if magic == b"!<arch>\n":
        shutil.copy2(source, destination)
    elif magic == b"!<thin>\n":
        members = run([ar, "t", source], source.parent, env, capture=True).splitlines()
        paths = [(source.parent / member).resolve() for member in members]
        if not paths or any(not member.is_file() for member in paths):
            raise RuntimeError(f"Invalid thin PDFium archive: {source}")
        # Batches avoid Windows command-line limits and preserve all members.
        for start in range(0, len(paths), 40):
            run([ar, "qcs", destination, *paths[start:start + 40]], source.parent, env)
    else:
        raise RuntimeError(f"Not a static archive: {source}")


def collect_notices(source, output):
    # Preserve all available third-party license texts from the pinned checkout.
    # Some in-tree libraries carry their notices in source headers instead.
    paths = {source / "LICENSE"}
    for root, dirs, files in os.walk(source / "third_party"):
        dirs[:] = sorted(d for d in dirs if d not in {".git", "__pycache__"})
        for name in files:
            if name.upper().startswith(("LICENSE", "COPYING", "COPYRIGHT")) or name == "README.ijg":
                paths.add(Path(root) / name)
    headers = ["agg23/agg_array.h", "lcms/include/lcms2.h", "libopenjpeg/openjpeg.c",
               "libtiff/t4.h", "zlib/zlib.h"]
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("w", encoding="utf-8") as result:
        for path in sorted(paths):
            if path.is_file():
                result.write(f"----- {path.relative_to(source)} -----\n")
                result.write(path.read_text(encoding="utf-8", errors="replace") + "\n")
        for name in headers:
            path = source / "third_party" / name
            if path.is_file():
                text = path.read_text(encoding="utf-8", errors="replace")
                end = text.find("*/")
                if end >= 0:
                    result.write(f"----- {path.relative_to(source)} -----\n{text[:end + 2]}\n")


def build_sdk(root, sdk, manifest, target, expected):
    spec = manifest["targets"][target]
    host = {"Darwin": "mac", "Linux": "linux", "Windows": "win"}.get(platform.system())
    if host != spec["os"]:
        raise RuntimeError(f"Build PDFium {target} on its native OS; this host is {host}")
    source, env, python = source_tree(root, manifest, target)
    if host == "win":
        env["VSCMD_ARG_TGT_ARCH"] = spec["cpu"]
    platform_toolchain(source, env, host)
    build = source / "out" / "zg-static"
    build.mkdir(parents=True, exist_ok=True)
    (build / "args.gn").write_text("".join(
        f"{key} = {json.dumps(value)}\n" for key, value in sorted(gn_arguments(spec).items())))
    gn = source / "buildtools" / {"mac": "mac", "linux": "linux64", "win": "win"}[host] / ("gn.exe" if host == "win" else "gn")
    run([gn, "gen", build, "--script-executable=" + python], source, env)
    jobs = os.environ.get("CARGO_BUILD_JOBS") or str(min(os.cpu_count() or 1, 8))
    libraries = {"pdfium.lib" if host == "win" else "libpdfium.a": build / "obj" /
                 ("pdfium.lib" if host == "win" else "libpdfium.a")}
    targets = ["pdfium"]
    if host == "linux" and spec.get("compiler") != "gcc":
        for name in ["libc++", "libc++abi"]:
            relative = f"obj/buildtools/third_party/{name}/{name}.a"
            libraries[name + ".a"] = build / relative
            targets.append(relative)
    if spec.get("compiler") == "gcc":
        runtime = Path(run(["g++", "-print-file-name=libstdc++.a"], source, env, capture=True))
        if not runtime.is_file():
            raise RuntimeError("Static PDFium requires the GCC libstdc++ static archive (install libstdc++-static)")
        libraries["libstdc++.a"] = runtime.resolve()
    ninja = source / "third_party" / "ninja" / ("ninja.exe" if host == "win" else "ninja")
    run([ninja, "-C", build, "-j", jobs, *targets], source, env)
    ar = (shutil.which("ar") if spec.get("compiler") == "gcc" else
          source / "third_party" / "llvm-build" / "Release+Asserts" / "bin" / "llvm-ar")
    with tempfile.TemporaryDirectory(dir=sdk.parent) as temporary:
        staged = Path(temporary)
        for name, library in libraries.items():
            portable_archive(library, staged / "lib" / name, ar, env)
        collect_notices(source, staged / "licenses/pdfium-ThirdPartyNotices.txt")
        if spec.get("compiler") == "gcc":
            with (staged / "licenses/pdfium-ThirdPartyNotices.txt").open("a") as notices:
                notices.write(Path(__file__).with_name("licenses").joinpath("gcc-runtime.txt").read_text())
        files = {p.relative_to(staged).as_posix(): digest(p) for p in staged.rglob("*") if p.is_file()}
        (staged / MARKER).write_text(json.dumps({"identity": expected, "files": files}, indent=2) + "\n")
        if sdk.exists():
            shutil.rmtree(sdk)
        shutil.move(str(staged), sdk)


def link_flags(sdk, spec):
    flags = [f"cargo:rustc-link-search=native={sdk / 'lib'}", "cargo:rustc-link-lib=static=pdfium"]
    if spec["os"] == "mac":
        flags += ["cargo:rustc-link-lib=dylib=c++"]
        flags += [f"cargo:rustc-link-lib=framework={name}"
                  for name in ["CoreGraphics", "CoreFoundation", "CoreText", "AppKit"]]
    elif spec["os"] == "linux":
        if spec.get("compiler") == "gcc":
            flags += ["cargo:rustc-link-lib=static=stdc++"]
        else:
            flags += ["cargo:rustc-link-lib=static=c++", "cargo:rustc-link-lib=static=c++abi"]
        flags += [f"cargo:rustc-link-lib={name}" for name in ["pthread", "dl", "m"]]
    else:
        flags += [f"cargo:rustc-link-lib={name}"
                  for name in ["gdi32", "user32", "advapi32", "comdlg32", "shell32"]]
    return flags


def prepare(cache, target, profile=None, offline=False, manifest=None):
    manifest = manifest or json.loads(MANIFEST_PATH.read_text())
    if target not in manifest["targets"]:
        raise RuntimeError(f"Unsupported static PDFium target: {target}")
    root = cache.resolve() / manifest["version"] / target
    sdk = root / "sdk"
    expected = identity(manifest, target)
    with build_lock(root / "build.lock"):
        if not verified_sdk(sdk, expected):
            if offline:
                raise RuntimeError(f"Offline build requires a verified static PDFium SDK: {sdk}")
            build_sdk(root, sdk, manifest, target, expected)
            if not verified_sdk(sdk, expected):
                raise RuntimeError("Static PDFium SDK verification failed")
        if profile:
            destination = profile / "licenses" / "pdfium-ThirdPartyNotices.txt"
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(sdk / "licenses" / destination.name, destination)
            for flag in link_flags(sdk, manifest["targets"][target]):
                print(flag)
            for path in [sdk / MARKER, *sorted((sdk / "lib").iterdir()), destination]:
                print(f"cargo:rerun-if-changed={path}")
    print(f"Verified static PDFium {manifest['version']} {target}: {sdk}", file=sys.stderr)
    return sdk


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--cache-dir", type=Path)
    parser.add_argument("--cargo-out-dir", type=Path)
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    profile = args.cargo_out_dir.resolve().parents[2] if args.cargo_out_dir else None
    cache = args.cache_dir or (profile.parent if profile else Path("target")) / "pdfium-static"
    prepare(cache, args.target, profile, args.offline or os.environ.get("CARGO_NET_OFFLINE", "").lower() in {"1", "true"})


if __name__ == "__main__":
    main()
