import assert from "node:assert/strict";
import { readFile, realpath } from "node:fs/promises";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { inside, sha256 } from "../core/io.mjs";
import { relativePath } from "../core/response.mjs";
import { summarizeDiagnosticRetrieval } from "../metrics/entities.mjs";
import { PILOT_SUITES_BY_ID } from "./config.mjs";

const goldDirectory = fileURLToPath(new URL("./gold/", import.meta.url));
const hashPattern = /^[a-f0-9]{64}$/;

export function validatePilotGold(gold, lock, lockBytes) {
  assert.equal(gold.schema_version, PILOT_SUITES_BY_ID[lock.suite].goldVersion);
  assert.equal(gold.suite, lock.suite);
  assert.equal(
    gold.source_lock_sha256,
    sha256(lockBytes),
    "Gold source lock changed",
  );
  assert.deepEqual(
    Object.keys(gold.tasks).sort(),
    lock.tasks.map((task) => task.id).sort(),
  );

  for (const task of lock.tasks) {
    const entry = gold.tasks[task.id];
    const originals = new Map(
      task.qrels
        ? task.qrels.map(({ document_id }) => [
            document_id,
            { path: `docs/${document_id}.md` },
          ])
        : task.positive_units.map((unit) => [unit.unit_key, unit]),
    );
    const expectedPaths = [
      ...new Set([...originals.values()].map(({ path }) => path)),
    ];
    assert.deepEqual(
      entry.files.map(({ path }) => path),
      expectedPaths,
    );
    for (const file of entry.files) {
      assert.ok(relativePath(file.path), "Gold requires a relative path");
      assert.match(file.sha256, hashPattern);
      assert.ok(Number.isSafeInteger(file.bytes) && file.bytes > 0);
    }
    const files = new Map(entry.files.map((file) => [file.path, file]));
    const seen = new Set();
    const spans = new Set();
    for (const target of entry.targets) {
      const original = originals.get(target.source_id);
      assert.equal(
        target.path,
        original?.path,
        "Gold target does not match its original label",
      );
      assert.ok(!seen.has(target.source_id), "Duplicate Gold source ID");
      seen.add(target.source_id);
      assert.ok(typeof target.reason === "string" && target.reason.trim());
      const grouped = target.evidence_sets !== undefined;
      if (nameIsScifact(task.id))
        assert.ok(grouped, "SciFact requires its original evidence sets");
      if (grouped) {
        assert.ok(
          gold.schema_version >= 2,
          "Evidence sets require Gold schema 2",
        );
        assert.ok(
          Array.isArray(target.evidence_sets) && target.evidence_sets.length,
          "Empty evidence alternatives",
        );
        for (const field of [
          "start_line",
          "end_line",
          "start_byte",
          "end_byte",
          "sha256",
        ])
          assert.equal(
            target[field],
            undefined,
            "Grouped target also has a flat span",
          );
      }
      const ids = new Set(),
        alternatives = new Set();
      for (const group of grouped
        ? target.evidence_sets
        : [{ spans: [target] }]) {
        if (grouped) {
          assert.ok(
            typeof group.id === "string" &&
              group.id.trim() &&
              !ids.has(group.id),
            "Missing or duplicate evidence set ID",
          );
          ids.add(group.id);
        }
        assert.ok(
          Array.isArray(group.spans) && group.spans.length,
          "Empty evidence set",
        );
        const members = new Set();
        for (const range of group.spans) {
          const { start_line, end_line, start_byte, end_byte } = range;
          assert.ok(
            [start_line, end_line, start_byte, end_byte].every(
              Number.isSafeInteger,
            ),
          );
          assert.ok(start_line > 0 && end_line >= start_line);
          assert.ok(
            start_byte >= 0 &&
              end_byte > start_byte &&
              end_byte <= files.get(target.path).bytes,
          );
          if (original.start_line !== undefined) {
            assert.equal(start_line, original.start_line);
            assert.equal(end_line, original.end_line);
          }
          const span = JSON.stringify([target.path, start_byte, end_byte]);
          assert.ok(
            !members.has(span),
            "Duplicate span within an evidence set",
          );
          members.add(span);
          if (!grouped) {
            assert.ok(!spans.has(span), "Duplicate Gold source span");
            spans.add(span);
          }
          assert.match(range.sha256, hashPattern);
        }
        const alternative = JSON.stringify([...members].sort());
        assert.ok(
          !alternatives.has(alternative),
          "Duplicate evidence alternative",
        );
        alternatives.add(alternative);
        if (nameIsScifact(task.id)) {
          assert.ok(Array.isArray(group.scifact_sentences));
          assert.equal(group.scifact_sentences.length, group.spans.length);
          assert.ok(
            group.scifact_sentences.every(
              (id) => Number.isSafeInteger(id) && id >= 0,
            ),
          );
          assert.equal(
            new Set(group.scifact_sentences).size,
            group.scifact_sentences.length,
          );
        }
      }
    }
    for (const { source_id, reason } of entry.unresolved) {
      assert.ok(
        originals.has(source_id) && !seen.has(source_id),
        "Invalid unresolved Gold label",
      );
      assert.ok(typeof reason === "string" && reason.trim());
      seen.add(source_id);
    }
    assert.equal(
      seen.size,
      originals.size,
      `Unaccounted Gold labels: ${task.id}`,
    );
  }
}

const nameIsScifact = (id) => id.startsWith("scifact/");

export async function loadPilotGold(name, lock, lockBytes) {
  const bytes = await readFile(join(goldDirectory, `${name}.json`));
  const gold = JSON.parse(bytes);
  validatePilotGold(gold, lock, lockBytes);
  return {
    identity: {
      id: `${lock.suite}-entity-v${gold.schema_version}`,
      sha256: sha256(bytes),
    },
    tasks: gold.tasks,
  };
}

/** Verify all labeled source files and evidence bytes before indexing. */
export async function verifyPilotGoldSources(gold, group) {
  const files = new Map();
  const root = await realpath(group.root);
  for (const task of group.tasks) {
    const entry = gold.tasks[task.id];
    for (const file of entry.files) {
      if (!files.has(file.path)) {
        const path = await realpath(resolve(root, file.path));
        assert.ok(
          inside(root, path),
          `Gold path escapes the corpus: ${file.path}`,
        );
        const bytes = await readFile(path);
        assert.equal(
          bytes.length,
          file.bytes,
          `Changed Gold file size: ${file.path}`,
        );
        assert.equal(
          sha256(bytes),
          file.sha256,
          `Changed Gold file: ${file.path}`,
        );
        const source = new TextDecoder("utf-8", {
          fatal: true,
          ignoreBOM: true,
        }).decode(bytes);
        let offset = 0;
        const lines = source.split("\n").map((raw) => {
          const line = { text: raw.replace(/\r$/, ""), offset };
          offset += Buffer.byteLength(raw) + 1;
          return line;
        });
        files.set(file.path, { bytes, lines });
      }
    }
    for (const target of entry.targets) {
      const { bytes, lines } = files.get(target.path);
      const ranges = target.evidence_sets?.flatMap((group) => group.spans) ?? [
        target,
      ];
      for (const range of ranges) {
        const { start_byte, end_byte, start_line, end_line } = range;
        assert.ok(start_line <= lines.length && end_line <= lines.length);
        assert.ok(
          lines[start_line - 1].offset <= start_byte &&
            start_byte < (lines[start_line]?.offset ?? bytes.length) &&
            lines[end_line - 1].offset < end_byte &&
            end_byte <= (lines[end_line]?.offset ?? bytes.length),
          `Gold byte and line positions disagree: ${target.source_id}`,
        );
        assert.equal(
          sha256(bytes.subarray(start_byte, end_byte)),
          range.sha256,
          `Changed Gold span: ${target.source_id}`,
        );
      }
    }
  }
  return files;
}

/**
 * Locate the full returned Entity in the verified source. Compact MCP headers
 * only expose line numbers. A unique alignment supplies the missing columns.
 */
export function locateFullEntity(item, file) {
  if (!file || item.range?.kind !== "text") return null;
  const { start_line, end_line } = item.range;
  if (end_line > file.lines.length) return null;
  const visible = item.source_lines.filter(({ line }) => line <= end_line);
  if (visible.length !== end_line - start_line + 1) return null;
  for (const [i, row] of visible.entries()) {
    if (row.line !== start_line + i) return null;
    const text = file.lines[row.line - 1].text;
    if (i > 0 && i < visible.length - 1 && row.text !== text) return null;
  }
  const first = file.lines[start_line - 1];
  const last = file.lines[end_line - 1];
  const head = visible[0].text;
  const tail = visible.at(-1).text;
  if (first === last) {
    const start = first.text.indexOf(head);
    if (!head || start < 0 || first.text.indexOf(head, start + 1) >= 0)
      return null;
    const startByte =
      first.offset + Buffer.byteLength(first.text.slice(0, start));
    return {
      start_byte: startByte,
      end_byte: startByte + Buffer.byteLength(head),
    };
  }
  if (!first.text.endsWith(head) || !last.text.startsWith(tail)) return null;
  return {
    start_byte:
      first.offset +
      Buffer.byteLength(first.text.slice(0, first.text.length - head.length)),
    end_byte: last.offset + Buffer.byteLength(tail),
  };
}

export function summarizePilotEntity(calls, entry) {
  return summarizeDiagnosticRetrieval(
    calls,
    {
      file_targets: entry.files.map((file) => ({ ...file, id: file.path })),
      entity_targets: entry.targets.map((target) => ({
        ...target,
        id: target.source_id,
      })),
    },
    { entityByteRanges: true },
  );
}
