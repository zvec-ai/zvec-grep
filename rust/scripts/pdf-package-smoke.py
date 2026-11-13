#!/usr/bin/env python3
"""Verify PDF indexing/search using an installed native package (no build tools)."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tempfile
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--model-cache", type=Path, required=True)
    parser.add_argument("--logs", type=Path, required=True)
    parser.add_argument("--sample", type=Path, help="Optional local copy of the pinned TraceMonkey PDF")
    args = parser.parse_args()
    logs = args.logs.resolve()
    logs.mkdir(parents=True, exist_ok=True)
    bundle = args.bundle.resolve()
    binary = next((p for p in [bundle / "bin/zg.exe", bundle / "bin/zg"] if p.is_file()), None)
    check(binary is not None, f"No installed zg under {bundle}")
    forbidden = [str(p) for p in bundle.rglob("*") if re.fullmatch(r"(?:libpdfium\.(?:so(?:\..*)?|dylib|a)|pdfium\.(?:dll|lib))", p.name, re.I)]
    check(not forbidden, f"Unexpected PDFium runtime/archive in package: {forbidden}")
    check((bundle / "lib/licenses/pdfium-ThirdPartyNotices.txt").is_file(), "Missing PDFium notices")
    data = args.sample.read_bytes() if args.sample else urllib.request.urlopen(SAMPLE_URL, timeout=60).read()
    check(hashlib.sha256(data).hexdigest() == SAMPLE_SHA, "Pinned real PDF checksum mismatch")
    cache = args.model_cache.resolve()
    cache.mkdir(parents=True, exist_ok=True)
    # Isolate configuration and remove library overrides used by development builds.
    env = {k: v for k, v in os.environ.items() if not k.startswith(("DYLD_", "ZVEC_GREP_")) and k not in
           {"LD_LIBRARY_PATH", "LD_PRELOAD", "ORT_DYLIB_PATH", "GGML_BACKEND_PATH"}}
    home = logs / "home"
    home.mkdir(exist_ok=True)
    env.update(HOME=str(home), USERPROFILE=str(home), XDG_CONFIG_HOME=str(home / "config"),
               ZVEC_GREP_MODEL_CACHE=str(cache), ZVEC_GREP_DEVICE="cpu", ZVEC_GREP_MODE="direct")
    report = {"passed": False, "platform": platform.platform(), "binary": str(binary),
              "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "sample": {"url": SAMPLE_URL, "sha256": SAMPLE_SHA, "pages": 14}, "commands": []}
    with tempfile.TemporaryDirectory(prefix="zg-pdf-package-") as temporary:
        workspace = Path(temporary)
        (workspace / "paper.pdf").write_bytes(data)
        (workspace / "notes.md").write_text("# Recovery\n\nOrchid recovery requires five minutes.\n", encoding="utf-8")

        def execute(label, arguments, codes=(0,)):
            started = time.perf_counter()
            command = [str(binary), *arguments, "--mode", "direct", "--color", "never"]
            child = subprocess.run(command, cwd=workspace, env=env, text=True, encoding="utf-8", errors="replace", capture_output=True, timeout=300)
            elapsed = time.perf_counter() - started
            (logs / f"{label}.stdout").write_text(child.stdout, encoding="utf-8")
            (logs / f"{label}.stderr").write_text(child.stderr, encoding="utf-8")
            report["commands"].append({"label": label, "args": arguments, "seconds": round(elapsed, 3), "exit_code": child.returncode})
            check(child.returncode in codes, f"{label}: exit {child.returncode}; {child.stderr[-2500:]}")
            return child.stdout, child.stderr

        index_args = ["--index", "--embedding", "local/all-minilm-l6-v2", "--model-cache", str(cache), "--device", "cpu"]
        try:
            out, _ = execute("index-real", index_args)
            check("2 scanned" in out and "0 failed" in out, out)
            out, _ = execute("index-unchanged", ["--index"])
            check("2 unchanged" in out, out)
            for preview in ["none", "short", "full", "detail"]:
                out, _ = execute("query-" + preview, ["Trace-based Just-in-Time Type Specialization", "-t", "pdf", "--preview", preview, "--limit", "1", "--refresh", "off"])
                check("paper.pdf" in out and "text/native" in out, out)
                check(bool(re.search(r"pages:\s*1(?:\D|$)", out, re.I)), out)
                check(("Boxes:" in out) == (preview == "detail"), out)
                if preview == "detail":
                    boxes = re.findall(r"page: (\d+)\s+block: (\d+)\s+box: \[([^]]+)\]", out)
                    check(bool(boxes), "No PDF boxes")
                    for page, _, coords in boxes:
                        x0, y0, x1, y1 = map(float, coords.split(","))
                        check(1 <= int(page) <= 14 and 0 <= x0 <= x1 <= 1 and 0 <= y0 <= y1 <= 1, coords)
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
            _, err = execute("rg-unavailable", ["--rg", "-F", "Orchid recovery"], (1,))
            check("broken.pdf" in err and "zg --index" in err, err)
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
            report["passed"] = True
        finally:
            (logs / "results.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
