import { constants, cpSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { ZVecOpen, type ZVecCollection, type ZVecDoc } from "@zvec/zvec";
import { resolveWorkspaceIndexStoragePaths } from "./layout.js";

// Callers hold the source read lock across this read. Native readOnly opens
// can still change vector-index metadata. Open private copies, never the
// original files. Reflinks reduce copying where supported; no hard links.
export function readNativeTransferSource(sourceHome: string): {
  files: () => Iterable<ZVecDoc>;
  entities: (includeVector?: boolean) => Iterable<ZVecDoc>;
  counts: { files: number; entities: number };
  close: () => void;
} {
  const snapshot = mkdtempSync(join(tmpdir(), "zg-transfer-source-"));
  const handles: ZVecCollection[] = [];
  try {
    const source = resolveWorkspaceIndexStoragePaths(sourceHome);
    const copy = resolveWorkspaceIndexStoragePaths(snapshot);
    for (const key of ["filesPath", "indexPath"] as const) {
      cpSync(source[key], copy[key], {
        recursive: true,
        dereference: true,
        mode: constants.COPYFILE_FICLONE,
        force: false,
        errorOnExist: true,
      });
    }
    const files = open(copy.filesPath);
    const entities = open(copy.indexPath);
    return {
      files: function* () {
        const iterator = files.iterDocsSync({ includeVector: false });
        try {
          // Do not delegate with yield*: it calls native next(undefined).
          // Some 0.7 bindings enforce a zero-argument next() contract.
          for (const doc of iterator) yield doc;
        } finally {
          iterator.closeSync();
        }
      },
      entities: function* (includeVector = true) {
        const iterator = entities.iterDocsSync({ includeVector });
        try {
          for (const doc of iterator) yield doc;
        } finally {
          iterator.closeSync();
        }
      },
      counts: {
        files: files.stats.docCount,
        entities: entities.stats.docCount,
      },
      close: () => close(),
    };
  } catch (error) {
    close(error);
    throw error;
  }

  function close(cause?: unknown) {
    const errors: unknown[] = cause === undefined ? [] : [cause];
    for (const handle of handles.splice(0).reverse()) {
      try {
        handle.closeSync();
      } catch (error) {
        errors.push(error);
      }
    }
    try {
      rmSync(snapshot, { recursive: true, force: true });
    } catch (error) {
      errors.push(error);
    }
    if (errors.length === 1) throw errors[0];
    if (errors.length > 1) {
      throw new AggregateError(
        errors,
        `Transfer source read or cleanup failed: ${errors.map(String).join("; ")}`,
      );
    }
  }

  function open(path: string): ZVecCollection {
    const handle = ZVecOpen(path, { readOnly: true, enableMMAP: false });
    handles.push(handle);
    return handle;
  }
}
