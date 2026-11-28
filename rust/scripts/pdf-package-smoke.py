#!/usr/bin/env python3
"""Verify PDF indexing/search using an installed native package (no build tools)."""

import argparse
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tempfile
import threading
import time
import urllib.request

SAMPLE_URL = "https://raw.githubusercontent.com/mozilla/pdf.js/5b4187ca507e8b374bac7d87169df47391cff944/test/pdfs/tracemonkey.pdf"
SAMPLE_SHA = "3662ff519e485810520552bf301d8c3b2b917fd2f83303f4965d7abed367e113"


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def fixture(path, pages):
    """Small controlled source for page mapping and lifecycle, not quality scoring."""
    objects = [b"<< /Type /Catalog /Pages 2 0 R >>", b"", b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"]
    kids = []
    for lines in pages:
        page_id = len(objects) + 1
        kids.append(f"{page_id} 0 R")
        stream = b"BT /F1 12 Tf 72 700 Td 18 TL\n"
        for line in lines:
            escaped = line.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")
            stream += f"({escaped}) Tj T*\n".encode("ascii")
        stream += b"ET"
        objects += [f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {page_id + 1} 0 R >>".encode(),
                    b"<< /Length " + str(len(stream)).encode() + b" >>\nstream\n" + stream + b"\nendstream"]
    objects[1] = f"<< /Type /Pages /Count {len(kids)} /Kids [{' '.join(kids)}] >>".encode()
    data = bytearray(b"%PDF-1.4\n")
    offsets = [0]
    for number, obj in enumerate(objects, 1):
        offsets.append(len(data))
        data += f"{number} 0 obj\n".encode() + obj + b"\nendobj\n"
    xref = len(data)
    data += f"xref\n0 {len(offsets)}\n0000000000 65535 f \n".encode()
    data += b"".join(f"{offset:010d} 00000 n \n".encode() for offset in offsets[1:])
    data += f"trailer\n<< /Size {len(offsets)} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    path.write_bytes(data)


def remote_layout_cache_smoke(execute, workspace, home, env, cache, provider):
    """Every remote provider must preserve the independently cached layout model."""
    reference = "http/fixture" if provider == "http" else "qwen/text-embedding-v4"
    root = workspace / f"{provider}-cache"
    root.mkdir()
    fixture(root / "native.pdf", [["Orchid recovery requires five minutes."]])
    isolated_home = home / f"{provider}-cache"
    config_dir = isolated_home / ".zvec-grep"
    config_dir.mkdir(parents=True, exist_ok=True)
    unavailable_cache = isolated_home / "not-a-directory"
    unavailable_cache.write_text("An ignored --model-cache must fail without downloading.", encoding="utf-8")
    (config_dir / "models").write_text("Do not download models when a saved cache is lost.", encoding="utf-8")
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(request)
            inputs = request["input"]
            dimension = request.get("dimensions", 4)
            body = json.dumps({"data": [{"index": i, "embedding": [1.0] + [0.0] * (dimension - 1)}
                                        for i in range(len(inputs))]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            endpoint = f"http://127.0.0.1:{server.server_port}/v1/embeddings"
            config = {"version": 1}
            if provider == "http":
                config["models"] = {reference: {
                    "endpoint": endpoint, "model": "fixture", "dimension": 4,
                    "maxInputTokens": 512, "maxBatchSize": 16, "auth": "none"}}
            config_path = config_dir / "config.json"
            config_path.write_text(json.dumps(config), encoding="utf-8")
            isolated_env = dict(env, HOME=str(isolated_home), USERPROFILE=str(isolated_home),
                                XDG_CONFIG_HOME=str(isolated_home / "config"),
                                ZVEC_GREP_MODEL_CACHE=str(unavailable_cache))
            isolated_env.pop("ZVEC_GREP_DEVICE", None)
            arguments = ["--index", "--embedding", reference, "--allow-remote"]
            if provider == "qwen":
                arguments += ["--endpoint", endpoint, "--api-key", "fixture-token"]

            def index(label, args, expected, cwd=root):
                before = len(requests)
                out, err = execute(label, args, cwd=cwd, environment=isolated_env)
                check(all(part in out for part in expected) and "0 failed" in out, out + err)
                check("Preparing model: layout/pp-doclayout-s" in err, err)
                check("using native PDF text without layout filtering" not in err, err)
                check("Downloading model" not in err, "Expected cached PP-S: " + err)
                check(len(requests) > before, f"{provider} embedding fixture received no request")

            def saved_cache(cwd=root):
                manifest = json.loads((cwd / ".zvec-grep/manifest.json").read_text(encoding="utf-8"))
                check(manifest["sourceRoot"]["recordedUsername"] == "zg-pdf-package-smoke",
                      "Saved cache reuse requires the same known fixture user")
                check(all("cacheDir" not in model for model in
                          manifest["index"]["descriptor"]["embeddingConfigs"].values()),
                      "Model cache must be stored once at workspace level")
                return manifest.get("modelCacheDir")

            index(f"{provider}-cache-explicit", arguments + ["--model-cache", str(cache)], ["1 scanned", "1 added"])
            check(saved_cache() == str(cache), "Explicit model cache was not saved")
            # A fresh process must reuse the workspace preference without any
            # explicit cache, environment fallback, or global configuration.
            isolated_env.pop("ZVEC_GREP_MODEL_CACHE", None)
            config_path.unlink()
            fixture(root / "added.pdf", [["Cobalt verification completes the recovery."]])
            index(f"{provider}-cache-incremental", ["--index", "--allow-remote"], ["2 scanned", "1 added", "1 unchanged"])
            check(saved_cache() == str(cache), "Incremental index lost the saved model cache")
            out, _ = execute(f"{provider}-cache-rg", ["--rg", "-F", "Cobalt verification"], cwd=root, environment=isolated_env)
            check("added.pdf" in out and re.search(r"pages:\s*1\s*\n", out), out)

            if provider == "http":
                # Configure only model B, with no global cache preference. The
                # workspace cache must survive selecting B instead of A.
                replacement = "http/fixture-b"
                replacement_config = {"version": 1, "models": {replacement: {
                    **config["models"][reference], "model": "fixture-b"}}}
                config_path.write_text(json.dumps(replacement_config), encoding="utf-8")
                index("http-cache-switch-rebuild",
                      ["--index", "--rebuild", "--embedding", replacement, "--allow-remote"],
                      ["2 scanned", "2 added"])
                check(saved_cache() == str(cache), "Switching models lost the workspace cache")
                check(requests[-1]["model"] == "fixture-b", "Rebuild did not use model B")
                config_path.unlink()
                fixture(root / "third.pdf", [["Amber confirmation finishes the checklist."]])
                index("http-cache-switch-incremental", ["--index", "--allow-remote"],
                      ["3 scanned", "1 added", "2 unchanged"])
                check(saved_cache() == str(cache), "Model B did not retain the workspace cache")
                check(requests[-1]["model"] == "fixture-b", "Incremental index did not use model B")

            # Exercise the global fallback in a separate workspace with no saved preference.
            global_root = workspace / f"{provider}-global-cache"
            global_root.mkdir()
            fixture(global_root / "native.pdf", [["Orchid recovery requires five minutes."]])
            config["defaults"] = {"modelCacheDir": str(cache)}
            config_path.write_text(json.dumps(config), encoding="utf-8")
            index(f"{provider}-cache-global", arguments, ["1 scanned", "1 added"], cwd=global_root)
            check(saved_cache(global_root) is None, "Global cache must not become a saved override")
        finally:
            server.shutdown()
            worker.join()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--npm-prefix", type=Path, help="Exercise the installed npm entry instead of the package binary")
    parser.add_argument("--model-cache", type=Path, required=True)
    parser.add_argument("--logs", type=Path, required=True)
    parser.add_argument("--sample", type=Path, help="Optional local copy of the pinned TraceMonkey PDF")
    args = parser.parse_args()
    logs = args.logs.resolve()
    logs.mkdir(parents=True, exist_ok=True)
    bundle = args.bundle.resolve()
    binary = next((p for p in [bundle / "bin/zg.exe", bundle / "bin/zg"] if p.is_file()), None)
    check(binary is not None, f"No installed zg under {bundle}")
    command_prefix = [str(binary)]
    entry = str(binary)
    if args.npm_prefix:
        # Reuse the release harness's actual npm entry (including the Windows
        # PowerShell shim). Do not resolve the entry symlink before execution.
        helper = Path(__file__).with_name("npm-installed-cli.mjs").resolve().as_uri()
        invocation = json.loads(subprocess.check_output([
            "node", "--input-type=module", "-e",
            "const {installedCliInvocation}=await import(process.argv[1]);"
            "console.log(JSON.stringify(installedCliInvocation(process.argv[2], [])));",
            helper, str(args.npm_prefix.absolute()),
        ], text=True, encoding="utf-8", timeout=30))
        entry = invocation["entry"]
        check(Path(entry).is_file(), f"Missing installed npm entry: {entry}")
        command_prefix = [invocation["command"], *invocation["args"]]
    library = "libpdfium.dylib" if platform.system() == "Darwin" else "pdfium.dll" if platform.system() == "Windows" else "libpdfium.so"
    check((bundle / "lib" / library).is_file(), "Missing bundled LiteParse PDFium")
    check((bundle / "lib/licenses/pdfium-ThirdPartyNotices.txt").is_file(), "Missing PDFium notices")
    data = args.sample.read_bytes() if args.sample else urllib.request.urlopen(SAMPLE_URL, timeout=60).read()
    check(hashlib.sha256(data).hexdigest() == SAMPLE_SHA, "Pinned real PDF checksum mismatch")
    cache = args.model_cache.resolve()
    cache.mkdir(parents=True, exist_ok=True)
    # Isolate configuration and remove library overrides used by development builds.
    env = {k: v for k, v in os.environ.items() if not k.startswith(("DYLD_", "ZVEC_GREP_")) and k not in
           {"LD_LIBRARY_PATH", "LD_PRELOAD", "ORT_DYLIB_PATH", "GGML_BACKEND_PATH", "PDFIUM_LIB_PATH", "PDFIUM_INCLUDE_PATH"}}
    home = logs / "home"
    home.mkdir(exist_ok=True)
    # Saved workspace caches require the same known user; containers may omit
    # both username variables, so give every fixture process a stable identity.
    env.update(HOME=str(home), USERPROFILE=str(home), XDG_CONFIG_HOME=str(home / "config"),
               USER="zg-pdf-package-smoke", USERNAME="zg-pdf-package-smoke",
               ZVEC_GREP_MODEL_CACHE=str(cache), ZVEC_GREP_DEVICE="cpu", ZVEC_GREP_MODE="direct")
    report = {"passed": False, "platform": platform.platform(), "binary": str(binary), "entry": entry,
              "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "sample": {"url": SAMPLE_URL, "sha256": SAMPLE_SHA, "pages": 14}, "commands": []}
    with tempfile.TemporaryDirectory(prefix="zg-pdf-package-") as temporary:
        workspace = Path(temporary)
        (workspace / "paper.pdf").write_bytes(data)
        (workspace / "notes.md").write_text("# Recovery\n\nOrchid recovery requires five minutes.\n", encoding="utf-8")

        def execute(label, arguments, codes=(0,), *, cwd=None, environment=None):
            started = time.perf_counter()
            command = [*command_prefix, *arguments, "--mode", "direct", "--color", "never"]
            child = subprocess.run(command, cwd=cwd or workspace, env=environment or env, text=True, encoding="utf-8", errors="replace", capture_output=True, timeout=300)
            elapsed = time.perf_counter() - started
            (logs / f"{label}.stdout").write_text(child.stdout, encoding="utf-8")
            (logs / f"{label}.stderr").write_text(child.stderr, encoding="utf-8")
            report["commands"].append({"label": label, "args": arguments, "seconds": round(elapsed, 3), "exit_code": child.returncode})
            check(child.returncode in codes, f"{label}: exit {child.returncode}; {child.stderr[-2500:]}")
            return child.stdout, child.stderr

        index_args = ["--index", "--embedding", "local/all-minilm-l6-v2", "--model-cache", str(cache), "--device", "cpu"]
        try:
            out, err = execute("index-real", index_args)
            check("using native PDF text without layout filtering" not in err, err)
            check("2 scanned" in out and "0 failed" in out, out)
            out, _ = execute("index-unchanged", ["--index"])
            check("2 unchanged" in out, out)
            for preview in ["none", "short", "full", "detail"]:
                out, _ = execute("query-" + preview, ["Trace-based Just-in-Time Type Specialization", "-t", "pdf", "--preview", preview, "--limit", "1", "--refresh", "off"])
                check("paper.pdf" in out, out)
                check(("text/native" in out) == (preview == "detail"), out)
                check(bool(re.search(r"pages:\s*1(?:\D|$)", out, re.I)), out)
                check(("Boxes:" in out) == (preview == "detail"), out)
                if preview == "detail":
                    groups = re.findall(r"^    Page (\d+): width: (\S+)  height: (\S+)\n((?:      x: [^\n]+\n)+)", out, re.M)
                    check(bool(groups), "No PDF boxes grouped by page")
                    check(len({page for page, *_ in groups}) == len(groups), "Repeated PDF page group")
                    check("block:" not in out and "normalized 0..1" not in out, "Internal box metadata leaked")
                    for page, page_width, page_height, rows in groups:
                        dimensions = [float(page_width), float(page_height)]
                        check(1 <= int(page) <= 14 and all(math.isfinite(v) and v > 0 for v in dimensions), page)
                        boxes = re.findall(r"x: (\S+)  y: (\S+)  width: (\S+)  height: (\S+)", rows)
                        check(bool(boxes), "No raw rectangles")
                        for coords in boxes:
                            x, y, width, height = map(float, coords)
                            check(all(math.isfinite(v) for v in (x, y, width, height)) and width >= 0 and height >= 0, coords)
                        check(any(abs(float(x)) > 1 or abs(float(y)) > 1 for x, y, _, _ in boxes), "Boxes unexpectedly normalized")
            out, _ = execute("rg-real", ["--rg", "-F", "Andreas Gal", "-t", "pdf"])
            check("paper.pdf" in out and re.search(r"pages:\s*1\s*\n", out, re.I), out)

            fixture(workspace / "pages.pdf", [["Orchid recovery requires five minutes.", "Keep the original journal before retrying."],
                                            ["Cobalt verification completes the recovery.", "Confirm the journal before accepting more requests."]])
            fixture(workspace / "empty.pdf", [[]])
            (workspace / "broken.pdf").write_bytes(b"%PDF-1.7\ntruncated document")
            out, err = execute("index-isolation", ["--index"], (0, 1))
            check("1 failed" in out and "broken.pdf" in out + err, out + err)
            check("empty.pdf" in out + err and "OCR is not enabled" in out + err, out + err)
            out, _ = execute("query-isolation", ["Orchid recovery", "--preview", "short", "--limit", "3", "--refresh", "off"])
            check("notes.md" in out and "pages.pdf" in out and "broken.pdf" not in out, out)
            out, err = execute("rg-unavailable", ["--rg", "-F", "Orchid recovery"])
            check("pages.pdf" in out and "notes.md" in out, out)
            check("zg --index" in err and "searching 1 files as raw content" in err, err)
            out, err = execute("rg-explicit-unavailable", ["--rg", "-F", "truncated document", "broken.pdf"])
            check("broken.pdf" in out and "truncated document" in out, out)
            check("pages:" not in out.lower() and "zg --index" in err, out + err)
            out, _ = execute("rg-mixed", ["--rg", "-F", "Orchid recovery", "-g", "!broken.pdf"])
            check("pages.pdf" in out and "notes.md" in out and re.search(r"pages:\s*1\s*\n", out), out)
            out, _ = execute("rg-page-two", ["--rg", "-F", "Cobalt verification", "-t", "pdf", "-g", "!broken.pdf"])
            check("pages.pdf" in out and re.search(r"pages:\s*2\s*\n", out), out)
            fixture(workspace / "broken.pdf", [["Repaired document preserves searchable recovery instructions."]])
            fixture(workspace / "pages.pdf", [["Updated orchid instructions replace the earlier waiting period."]])
            out, _ = execute("index-retry-modified", ["--index"])
            check("0 failed" in out and "1 retried" in out and "1 modified" in out, out)
            out, _ = execute("rg-repaired", ["--rg", "-F", "Repaired document"])
            check("broken.pdf" in out, out)
            out, _ = execute("rg-stale", ["--rg", "-F", "Cobalt verification"], (0, 1))
            check("pages.pdf" not in out, out)
            (workspace / "pages.pdf").unlink()
            out, _ = execute("index-deleted", ["--index"])
            check("1 deleted" in out, out)
            out, _ = execute("rg-deleted", ["--rg", "-F", "Updated orchid"], (0, 1))
            check("pages.pdf" not in out, out)
            for provider in ["http", "qwen"]:
                remote_layout_cache_smoke(execute, workspace, home, env, cache, provider)
            report["passed"] = True
        finally:
            (logs / "results.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
