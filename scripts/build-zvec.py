"""Build the pinned zvec C API and Node binding; never substitute a release SDK."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
ZVEC_COMMIT = "06076297c1ca34cc7edc2fa46dff48fc02f6da06"
NODE_COMMIT = "51c542807815a90275a5a14066a575cf991c65d8"


def run(args, **kwargs):
    subprocess.run([str(x) for x in args], check=True, **kwargs)


def output(args, **kwargs):
    return subprocess.check_output([str(x) for x in args], text=True, **kwargs).strip()


def source(path, url, commit):
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        run(["git", "clone", "--no-checkout", url, path])
        run(["git", "-C", path, "checkout", "--detach", commit])
    if output(["git", "-C", path, "rev-parse", "HEAD"]) != commit:
        raise SystemExit(f"Expected {path} at {commit}; refusing a different native runtime")
    if output(["git", "-C", path, "status", "--porcelain", "--untracked-files=no"]):
        raise SystemExit(f"Native source has uncommitted changes: {path}")


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--source", type=Path, default=ROOT.parent / "zvec")
parser.add_argument("--node-source", type=Path, default=ROOT / ".cache/zvec-node")
parser.add_argument("--build-dir", type=Path, default=ROOT / ".cache/zvec-build")
parser.add_argument("--node-headers", type=Path,
                    default=Path(shutil.which("node")).resolve().parents[1] / "include/node")
parser.add_argument("--jobs", type=int, default=min(8, os.cpu_count() or 1))
args = parser.parse_args()
if sys.platform not in ("darwin", "linux"):
    raise SystemExit("This source-build entry point currently supports macOS and Linux")
src, node_src, build = (p.resolve() for p in (args.source, args.node_source, args.build_dir))
source(src, "https://github.com/richyreachy/zvec.git", ZVEC_COMMIT)
run(["git", "-C", src, "submodule", "update", "--init", "--recursive", "--depth", "1"])
source(node_src, "https://github.com/zvec-ai/zvec-node.git", NODE_COMMIT)
node_api = Path(output(["node", "-p", "require.resolve('node-addon-api/package.json')"], cwd=ROOT)).parent
if not (args.node_headers / "node_api.h").is_file():
    raise SystemExit("Node development headers missing; provide --node-headers")
run(["cmake", "-S", src, "-B", build, "-DCMAKE_BUILD_TYPE=Release",
     "-DBUILD_C_BINDINGS=ON", "-DBUILD_PYTHON_BINDINGS=OFF", "-DBUILD_TESTING=OFF",
     "-DBUILD_TOOLS=OFF", "-DBUILD_CPP_EXAMPLES=OFF", "-DBUILD_ZVEC_SHARED=OFF",
     "-DBUILD_ZVEC_CORE_SHARED=OFF", "-DBUILD_ZVEC_AILEGO_SHARED=OFF",
     f"-DCMAKE_PROJECT_zvec_INCLUDE={ROOT / 'scripts/zvec-node.cmake'}",
     f"-DZG_NODE_SOURCE={node_src}", f"-DZG_NODE_API={node_api}",
     f"-DZG_NODE_HEADERS={args.node_headers.resolve()}"])
run(["cmake", "--build", build, "--target", "zvec_c_api", "zvec_node_binding",
     "--parallel", args.jobs])
destination = ROOT / "native"
destination.mkdir(exist_ok=True)
library = "libzvec_c_api.dylib" if sys.platform == "darwin" else "libzvec_c_api.so"
files = {}
for name in [library, "zvec_node_binding.node"]:
    target = destination / name
    shutil.copy2(build / "lib" / name, target)
    if sys.platform == "darwin":
        run(["codesign", "--force", "--sign", "-", target])
    files[name] = hashlib.sha256(target.read_bytes()).hexdigest()
shutil.copy2(src / "LICENSE", destination / "LICENSE.zvec")
shutil.copy2(node_src / "LICENSE", destination / "LICENSE.zvec-node")
manifest = dict(zvec_commit=ZVEC_COMMIT, node_commit=NODE_COMMIT,
                platform=sys.platform, arch=output(["node", "-p", "process.arch"]), files=files)
(destination / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
print(json.dumps(manifest, indent=2))
