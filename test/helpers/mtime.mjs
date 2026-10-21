import { stat, utimes } from "node:fs/promises";

// utimes accepts plain numbers as seconds with sub-millisecond precision
// (Date truncates to integer milliseconds). Write/read jitter is absorbed by
// sweeping candidate offsets inside the desired millisecond bucket until the
// truncated read-back equals the stored index mtime.
const OFFSETS_MS = [0.5, 0.6, 0.7, 0.8, 0.9, 0.99, 0.3, 0.15, 1.0];

export async function restoreIndexedMtime(target, indexedMtimeMs) {
  const desired = Math.trunc(indexedMtimeMs);
  for (const offset of OFFSETS_MS) {
    await utimes(target, (desired + offset) / 1000, (desired + offset) / 1000);
    if (Math.trunc((await stat(target)).mtimeMs) === desired) {
      return;
    }
  }
  throw new Error(
    `mtime for ${target} did not reach stored index mtime ${desired}`,
  );
}
