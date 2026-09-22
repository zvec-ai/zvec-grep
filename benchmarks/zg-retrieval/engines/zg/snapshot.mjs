// Passive audit through the native candidate's public status command.
import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileHash, objectHash, run, writeJson } from "../../core/lib.mjs";

function required(pattern, text, label) {
  const match = pattern.exec(text);
  assert.ok(match, `native status omitted ${label}`);
  return match;
}

export class NativeIndexProductError extends Error {}

function requireSearchable(condition, message) {
  if (!condition) throw new NativeIndexProductError(message);
}

async function writeStatusEvidence(output, result) {
  await writeFile(join(output, "status.txt"), result.stdout);
  await writeFile(join(output, "status.stderr.txt"), result.stderr);
  await writeJson(join(output, "status-exit.json"), {
    code: result.code,
    signal: result.signal,
    timed_out: result.timed_out,
  });
}

export function parseNativeStatus(stdout) {
  const state = required(
    /^Workspace index: (ready|missing)$/m,
    stdout,
    "state",
  )[1];
  const root = required(/^Root: (.+)$/m, stdout, "root")[1];
  const indexPath = required(/^Index path: (.+)$/m, stdout, "index path")[1];
  const embedding = required(/^Embedding: (\S+)$/m, stdout, "embedding")[1];
  const files = required(
    /^Files: scanned=(\d+) indexed=(\d+) pending=(\d+) failed=(\d+)$/m,
    stdout,
    "file counts",
  );
  const entities = required(/^Entities: (\d+)$/m, stdout, "entity count")[1];
  const bytes = required(
    /^Indexed source size: (\d+) bytes$/m,
    stdout,
    "indexed source size",
  )[1];
  const status = {
    state,
    root,
    index_path: indexPath,
    embedding,
    files_scanned: Number(files[1]),
    files_indexed: Number(files[2]),
    files_pending: Number(files[3]),
    files_failed: Number(files[4]),
    entities_indexed: Number(entities),
    indexed_source_bytes: Number(bytes),
  };
  for (const [name, value] of Object.entries(status))
    if (typeof value === "number")
      assert.ok(Number.isSafeInteger(value) && value >= 0, `invalid ${name}`);
  requireSearchable(status.state === "ready", "native index is not ready");
  requireSearchable(
    status.files_pending === 0,
    "native index still has pending files",
  );
  requireSearchable(
    status.files_failed === 0,
    "native index contains failed files",
  );
  requireSearchable(
    status.files_indexed > 0,
    "native index contains no indexed files",
  );
  requireSearchable(
    status.entities_indexed > 0,
    "native index contains no entities",
  );
  return status;
}

export async function snapshotIndex({
  cli,
  root,
  output,
  env,
  runStatus = run,
}) {
  await mkdir(output, { recursive: true });
  let result;
  try {
    result = await runStatus(
      cli,
      ["--status", root, "--mode", "direct", "--check-ready", "--debug"],
      { env, cwd: root },
    );
  } catch (error) {
    if (error.result) await writeStatusEvidence(output, error.result);
    throw error;
  }
  await writeStatusEvidence(output, result);
  const status = parseNativeStatus(result.stdout);
  assert.equal(
    status.root,
    root,
    "native status resolved a different workspace root",
  );
  const identity = {
    embedding: status.embedding,
    files_scanned: status.files_scanned,
    files_indexed: status.files_indexed,
    files_pending: status.files_pending,
    files_failed: status.files_failed,
    entities_indexed: status.entities_indexed,
    indexed_source_bytes: status.indexed_source_bytes,
  };
  await writeJson(join(output, "status.json"), status);
  const summary = {
    schema_version: 2,
    kind: "rust-public-status",
    logical_content_sha256: objectHash(identity),
    files: status.files_indexed,
    fragments: status.entities_indexed,
    failed_files: [],
    identity_excludes: [
      "index storage layout",
      "index path",
      "locks and process state",
      "filesystem timestamps",
    ],
    stages: {
      corpus_scan: "available_as_public_aggregate_counts",
      persisted_chunks_and_vectors: "not_exposed_by_public_rust_interface",
      actual_embedding_inputs: "not_available",
      preselection_candidates: "not_available",
      fusion_and_selection: "not_available",
      final_visible_output: "available_in_raw_responses",
    },
  };
  summary.artifacts = Object.fromEntries(
    await Promise.all(
      ["status.txt", "status.json"].map(async (path) => [
        path,
        await fileHash(join(output, path)),
      ]),
    ),
  );
  await writeJson(join(output, "summary.json"), summary);
  return summary;
}
