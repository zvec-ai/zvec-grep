#!/usr/bin/env python3
"""Export reviewed QA candidates without upgrading their review status."""

import argparse
import ast
import hashlib
import json
import re
import tarfile
from pathlib import Path


HERE = Path(__file__).resolve().parent
DISPUTES = {
    "streamlink:14": "The question does not identify a plugin or platform. CMMedia/Kaltura and MDStrm use different multi-stage validation flows.",
    "django:32": "The question does not identify which User model it means. The auth User and the formset regression User have different consequences.",
    "streamlink:43": "The question does not identify an HLS platform. Kick and Twitch both provide relevant low-latency initialization behavior.",
}
REASON_TRANSLATIONS = {
    "entity-1c251b68b65c9e6d75cdac32": "The module-level parameter table declares that __deepcopy__ accepts one argument after self.",
}


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def read_json(path):
    return json.loads(path.read_text())


def rows(path):
    return [json.loads(line) for line in path.read_text().splitlines() if line]


def named_nodes(node, scope=(), parent=None):
    for child in ast.iter_child_nodes(node):
        if isinstance(child, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
            name = scope + (child.name,)
            kind = "class" if isinstance(child, ast.ClassDef) else "function"
            if kind == "function" and isinstance(parent, ast.ClassDef):
                kind = "method"
                if any(isinstance(d, ast.Name) and d.id == "property" for d in child.decorator_list):
                    kind = "property"
            yield ".".join(name), kind, child
            yield from named_nodes(child, name, child)
        else:
            if not scope and isinstance(child, ast.Assign):
                for target in child.targets:
                    if isinstance(target, ast.Name):
                        yield target.id, "module_assignment", child
            yield from named_nodes(child, scope, parent)


class SourceArchive:
    def __init__(self, path, expected):
        assert digest(path.read_bytes()) == expected["sha256"], f"Archive checksum mismatch: {path.name}"
        self.archive = tarfile.open(path)
        assert self.archive.pax_headers.get("comment") == expected["pax_commit"], "Archive commit mismatch"
        self.files = {}
        self.nodes = {}

    def read(self, path):
        assert not path.startswith("/") and ".." not in Path(path).parts
        if path not in self.files:
            member = self.archive.getmember(path)
            assert member.isfile(), f"Not a regular source file: {path}"
            with self.archive.extractfile(member) as stream:
                self.files[path] = stream.read()
        return self.files[path]

    def span(self, path, location):
        raw = self.read(path)
        assert digest(raw) == location["file_sha256"], f"File checksum mismatch: {path}"
        lines = raw.splitlines(keepends=True)
        start = sum(map(len, lines[:location["start_line"] - 1]))
        end = sum(map(len, lines[:location["end_line"]]))
        assert (start, end) == (location["start_byte"], location["end_byte"]), f"Span bounds mismatch: {path}"
        content = raw[start:end]
        assert digest(content) == location["sha256"], f"Span checksum mismatch: {path}"
        return content

    def declaration(self, target):
        path = target["path"]
        definition = target["definition"]
        raw = self.read(path)
        lines = raw.splitlines(keepends=True)
        if path.endswith(".py"):
            if path not in self.nodes:
                self.nodes[path] = list(named_nodes(ast.parse(raw, filename=path)))
            matches = []
            for symbol, kind, node in self.nodes[path]:
                starts = {node.lineno, *(d.lineno for d in getattr(node, "decorator_list", []))}
                if (symbol == target["symbol"] and kind == target["kind"]
                        and definition["start_line"] in starts
                        and definition["end_line"] == node.end_lineno):
                    matches.append(node)
            assert len(matches) == 1, f"Ambiguous source declaration: {path}::{target['symbol']}"
            node = matches[0]
            start = node.lineno
            # A single exact declaration line establishes the fixed-source name;
            # the full definition below supplies any additional public evidence.
            end = start
        else:
            assert path.endswith(".c") and target["kind"] == "function"
            body = self.span(path, definition)
            signature = body.split(b"{", 1)[0]
            assert re.search(rb"\b" + re.escape(target["symbol"].encode()) + rb"\s*\(", signature)
            assert body.rstrip().endswith(b"}")
            start = definition["start_line"]
            end = start + signature.count(b"\n")
        return start, end, b"".join(lines[start - 1:end]).decode("utf-8")

    def licenses(self, repository, commit):
        names = sorted(m.name for m in self.archive.getmembers()
                       if m.isfile() and ("/" not in m.name or m.name == "LICENSE/LICENSE")
                       and re.match(r"^(LICENSE|LICENCE|COPYING)([._-].*)?$", Path(m.name).name, re.I))
        assert names, f"Missing source license: {repository}"
        return [{"path": name, "sha256": digest(self.read(name)),
                 "text": self.read(name).decode("utf-8"),
                 "url": f"https://github.com/{repository}/blob/{commit}/{name}"}
                for name in names]


def export(release, source_lock, archive_root=None):
    source = read_json(source_lock)
    frozen_tasks = {task["task_id"]: task for task in source["tasks"]}
    original_queries = {row["task_id"]: row for row in rows(release / "queries.jsonl")}
    gold = {row["task_id"]: row for row in rows(release / "gold.jsonl")}
    targets = {row["target_id"]: row for row in rows(release / "targets.jsonl")}
    lock = read_json(release / "input-lock.json")
    assert set(frozen_tasks) == set(original_queries) == set(gold)
    archives = {}
    licenses = {}
    tasks = {}
    for task_id, frozen in frozen_tasks.items():
        question, labels = original_queries[task_id], gold[task_id]
        assert question["query"] == frozen["query"]
        assert question["query_sha256"] == frozen["query_sha256"] == digest(frozen["query"].encode("utf-8"))
        assert question["repository"] == labels["repository"] == frozen["repository"]
        assert question["commit"] == labels["commit"] == frozen["repository_commit"]
        repository, commit = frozen["repository"], frozen["repository_commit"]
        evidence = lock["source_archives"][task_id]
        assert evidence["pax_commit"] == commit
        if repository not in archives:
            archive_path = archive_root / f"{commit}.tar.gz" if archive_root else Path(evidence["path"])
            archives[repository] = SourceArchive(archive_path, evidence)
            licenses[repository] = {"commit": commit, "files": archives[repository].licenses(repository, commit)}
        archive = archives[repository]
        diagnostic = all(labels[level]["state"] == "reviewed_known_positive" for level in ["file", "entity"])
        case = {
            "query": frozen["query"], "query_sha256": frozen["query_sha256"],
            "repository": repository, "repository_commit": commit, "category": frozen["category"],
            "review": {"correctness": "reviewed" if diagnostic else "disputed", "completeness": "not_reviewed",
                       "source_status": {level: labels[level]["state"] for level in ["file", "entity"]}},
            "scoring_eligible": False, "diagnostic_eligible": diagnostic,
            "exclusion_reasons": ["completeness_review_required"] + ([] if diagnostic else ["ambiguous_question"]),
            "source_archive_sha256": evidence["sha256"], "file_targets": [], "entity_targets": [],
        }
        if "interpretations" in labels:
            interpretations = labels["interpretations"]
            case["interpretations"] = {**interpretations, "reason": DISPUTES[task_id]}
        case["disputed_candidates"] = {level: labels[level]["disputed_candidates"] for level in ["file", "entity"]}
        for level in ["file", "entity"]:
            for ident in labels[level]["target_ids"]:
                target = targets[ident]
                assert (target["task_id"], target["repository"], target["commit"], target["level"]) == (task_id, repository, commit, level)
                anchor = target["anchor"]
                anchor_text = archive.span(target["path"], anchor).decode("utf-8")
                result = {"id": ident, "path": target["path"], "file_sha256": target["file_sha256"],
                          "reason": REASON_TRANSLATIONS.get(ident, target["reason"]), "review": target["review"],
                          "source_url": f"https://github.com/{repository}/blob/{commit}/{target['path']}#L{anchor['start_line']}-L{anchor['end_line']}"}
                source_anchor = {key: anchor[key] for key in ["start_line", "end_line", "start_byte", "end_byte", "sha256"]}
                source_anchor["text"] = anchor_text
                if level == "file":
                    result["anchor"] = source_anchor
                else:
                    definition = target["definition"]
                    definition_text = archive.span(target["path"], definition).decode("utf-8")
                    declaration_start, declaration_end, declaration_text = archive.declaration(target)
                    result.update({"file_target_id": target["file_target_id"], "symbol": target["symbol"], "kind": target["kind"],
                                   "start_line": definition["start_line"], "end_line": definition["end_line"],
                                   "start_byte": definition["start_byte"], "end_byte": definition["end_byte"],
                                   "declaration_start_line": declaration_start, "declaration_end_line": declaration_end,
                                   "declaration_text": declaration_text, "definition_sha256": definition["sha256"],
                                   "definition_text": definition_text, "anchors": [source_anchor]})
                case[f"{level}_targets"].append(result)
        tasks[task_id] = case
    for archive in archives.values():
        archive.archive.close()
    return {
        "schema_version": 1, "id": "sweqa20-qa-pairs-v1", "status": "diagnostic_only",
        "provenance": {"source_release": "retrieval-only-sweqa20-gold-pilot-v2",
                       "source_files": {name: digest((release / name).read_bytes()) for name in
                                        ["release.json", "queries.jsonl", "gold.jsonl", "targets.jsonl", "review_ledger.jsonl", "review_provenance.json"]},
                       "source_lock_sha256": digest(source_lock.read_bytes()),
                       "source_validation": "Source archive commit, file bytes, anchor bytes and definition identity were verified during export.",
                       "review_scope": "Historical review confirmed selected positive targets. Completeness was not reviewed. The disputed cases remain excluded from diagnostic scores."},
        "source_licenses": licenses, "tasks": tasks,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release", required=True, type=Path)
    parser.add_argument("--source-lock", type=Path, default=HERE.parent / "data/source.lock.json")
    parser.add_argument("--archive-root", type=Path, help="Optional directory containing <full-commit>.tar.gz archives.")
    parser.add_argument("--output", type=Path, default=HERE / "qa-pairs-v1.json")
    args = parser.parse_args()
    value = export(args.release, args.source_lock, args.archive_root)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")
    print(f"Exported {len(value['tasks'])} QA pairs; formal scoring remains disabled.")


if __name__ == "__main__":
    main()
