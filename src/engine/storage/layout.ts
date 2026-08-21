import { existsSync, rmSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

const FILES_ZVEC = "files.zvec";
const ENTITIES_ZVEC = "index.zvec";
const MANIFEST = "manifest.json";
const GRAPH_DIRECTORY = "code-graph";
const GRAPH_DATABASE = "graph.sqlite";

export type WorkspaceIndexLayout = {
  storagePath: string;
  manifestPath: string;
  filesPath: string;
  indexPath: string;
  graphPath: string;
  graphDatabasePath: string;
};

export function resolveWorkspaceIndexLayout(
  storagePath: string,
): WorkspaceIndexLayout {
  const resolvedStoragePath = resolve(storagePath);
  const graphPath = join(resolvedStoragePath, GRAPH_DIRECTORY);
  return {
    storagePath: resolvedStoragePath,
    manifestPath: join(resolvedStoragePath, MANIFEST),
    filesPath: join(resolvedStoragePath, FILES_ZVEC),
    indexPath: join(resolvedStoragePath, ENTITIES_ZVEC),
    graphPath,
    graphDatabasePath: join(graphPath, GRAPH_DATABASE),
  };
}

export const resolveWorkspaceIndexStoragePaths = resolveWorkspaceIndexLayout;

export function hasWorkspaceIndexStorage(storagePath: string): boolean {
  const paths = resolveWorkspaceIndexLayout(storagePath);
  return existsSync(paths.filesPath) && existsSync(paths.indexPath);
}

export function deleteWorkspaceIndexStorage(storagePath: string): void {
  const paths = resolveWorkspaceIndexLayout(storagePath);
  for (const target of [paths.filesPath, paths.indexPath]) {
    if (dirname(target) !== paths.storagePath) {
      throw new Error("Workspace index data must be inside its storage path");
    }
    rmSync(target, { recursive: true, force: true });
  }
}

export function deleteWorkspaceIndexArtifacts(storagePath: string): void {
  const layout = resolveWorkspaceIndexLayout(storagePath);
  for (const target of [
    layout.manifestPath,
    layout.filesPath,
    layout.indexPath,
    layout.graphPath,
  ]) {
    if (dirname(target) !== layout.storagePath) {
      throw new Error("Workspace artifact must be inside its storage path");
    }
    rmSync(target, { recursive: true, force: true });
  }
}

export function workspaceIndexPath(storagePath: string): string {
  return resolveWorkspaceIndexLayout(storagePath).indexPath;
}
