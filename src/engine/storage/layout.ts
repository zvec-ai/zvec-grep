import { randomUUID } from "node:crypto";
import { existsSync, renameSync, rmSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

const FILES_ZVEC = "files.zvec";
const ENTITIES_ZVEC = "index.zvec";

type WorkspaceIndexStoragePaths = {
  storagePath: string;
  filesPath: string;
  indexPath: string;
};

export function resolveWorkspaceIndexStoragePaths(
  storagePath: string,
): WorkspaceIndexStoragePaths {
  const resolvedStoragePath = resolve(storagePath);
  return {
    storagePath: resolvedStoragePath,
    filesPath: join(resolvedStoragePath, FILES_ZVEC),
    indexPath: join(resolvedStoragePath, ENTITIES_ZVEC),
  };
}

export function hasWorkspaceIndexStorage(storagePath: string): boolean {
  const paths = resolveWorkspaceIndexStoragePaths(storagePath);
  return existsSync(paths.filesPath) && existsSync(paths.indexPath);
}

export function deleteWorkspaceIndexStorage(storagePath: string): void {
  const paths = resolveWorkspaceIndexStoragePaths(storagePath);
  for (const target of [paths.filesPath, paths.indexPath]) {
    if (dirname(target) !== paths.storagePath) {
      throw new Error("Workspace index data must be inside its storage path");
    }
    rmSync(target, { recursive: true, force: true });
  }
}

export function installWorkspaceIndexStorage(
  liveStoragePath: string,
  stagingStoragePath: string,
): void {
  const live = resolveWorkspaceIndexStoragePaths(liveStoragePath);
  const staging = resolveWorkspaceIndexStoragePaths(stagingStoragePath);
  if (live.storagePath === staging.storagePath) {
    throw new Error(
      "Workspace rebuild staging must be distinct from live storage",
    );
  }
  if (!existsSync(staging.filesPath) || !existsSync(staging.indexPath)) {
    throw new Error("Workspace rebuild staging is incomplete");
  }

  const suffix = `.old.${process.pid}.${randomUUID()}`;
  const pairs = [
    [live.filesPath, staging.filesPath],
    [live.indexPath, staging.indexPath],
  ] as const;
  const backups: { livePath: string; backup: string }[] = [];

  try {
    for (const [livePath, stagingPath] of pairs) {
      if (dirname(livePath) !== live.storagePath) {
        throw new Error("Workspace index data must be inside its storage path");
      }
      if (dirname(stagingPath) !== staging.storagePath) {
        throw new Error(
          "Workspace rebuild staging must stay inside staging storage",
        );
      }
      if (existsSync(livePath)) {
        const backup = `${livePath}${suffix}`;
        renameSync(livePath, backup);
        backups.push({ livePath, backup });
      }
      renameSync(stagingPath, livePath);
    }
  } catch (error) {
    for (let index = backups.length - 1; index >= 0; index--) {
      const { livePath, backup } = backups[index];
      if (existsSync(livePath)) {
        rmSync(livePath, { recursive: true, force: true });
      }
      renameSync(backup, livePath);
    }
    throw error;
  }

  for (const { backup } of backups) {
    rmSync(backup, { recursive: true, force: true });
  }
}

export function workspaceIndexPath(storagePath: string): string {
  return resolveWorkspaceIndexStoragePaths(storagePath).indexPath;
}
