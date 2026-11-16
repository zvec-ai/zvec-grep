import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { sha256 } from "./io.mjs";

export const goldPaths = (pair) => pair.file_targets.map(({ path }) => path);

// The control file stays outside the corpus. The daemon publishes a separate
// file atomically before replying to the unchanged public MCP request.
export async function goldStageSession(evidence, enabled) {
  if (!enabled) return null;
  const directory = join(evidence, "gold-stages");
  try {
    await mkdir(directory, { mode: 0o700 });
  } catch {
    return null;
  }
  const request = join(directory, "request.json");
  let sequence = 0;
  let active = true;
  return {
    env: { ZG_RETRIEVAL_GOLD_STAGE_DIR: directory },
    async before(query, mode, pair) {
      if (!active) return null;
      const id = ++sequence;
      try {
        await writeFile(request, JSON.stringify({ id, query, mode, paths: goldPaths(pair) }), {
          flag: "wx",
          mode: 0o600,
        });
        return id;
      } catch {
        active = false;
        return null;
      }
    },
    async after(id, transportError) {
      if (id === null) return null;
      try {
        await rm(request);
        // An interrupted call may still be running remotely. Stop arming the
        // sidecar for this daemon so subsequent repetitions cannot inherit it.
        if (transportError) {
          active = false;
          return null;
        }
        const bytes = await readFile(join(directory, `capture-${id}.json`));
        return { id, sha256: sha256(bytes) };
      } catch {
        return null;
      }
    },
  };
}

export async function readGoldStages(evidence, record) {
  if (!Number.isSafeInteger(record?.id) || record.id < 1) return null;
  try {
    const bytes = await readFile(join(evidence, "gold-stages", `capture-${record.id}.json`));
    if (sha256(bytes) !== record.sha256) return null;
    const capture = JSON.parse(bytes);
    return capture.id === record.id ? capture : null;
  } catch {
    return null;
  }
}
