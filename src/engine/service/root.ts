import { existsSync, rmSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { workspaceManifestPath } from "../manifest.js";

export const ZVEC_GREP_DIR = ".zvec-grep";
export const FILES_ZVEC = "files.zvec";
export const ENTITIES_ZVEC = "index.zvec";

export type WorkspaceIndexLocation = {
  root: string;
  home: string;
  manifestPath: string;
  filesPath: string;
  indexPath: string;
};

export function resolveZvecGrepRoot(root: string | undefined): string {
  return resolve(root ?? process.cwd());
}

export function workspaceHome(root: string): string {
  return join(resolve(root), ZVEC_GREP_DIR);
}

export function workspaceIndexLocation(root: string): WorkspaceIndexLocation {
  const resolvedRoot = resolve(root);
  const home = workspaceHome(resolvedRoot);

  return {
    root: resolvedRoot,
    home,
    manifestPath: workspaceManifestPath(home),
    filesPath: join(home, FILES_ZVEC),
    indexPath: join(home, ENTITIES_ZVEC),
  };
}

export function resetWorkspaceIndexStorage(
  location: WorkspaceIndexLocation,
): void {
  for (const target of [
    location.manifestPath,
    location.filesPath,
    location.indexPath,
  ]) {
    if (dirname(target) !== location.home) {
      throw new Error("Workspace index data must be inside its workspace home");
    }
    rmSync(target, { recursive: true, force: true });
  }
}

export function findNearestWorkspaceIndex(
  start: string,
): WorkspaceIndexLocation | null {
  return findNearestWorkspaceLocation(start, hasWorkspaceIndex);
}

export function findNearestWorkspace(
  start: string,
): WorkspaceIndexLocation | null {
  return findNearestWorkspaceLocation(start, hasWorkspaceManifest);
}

function findNearestWorkspaceLocation(
  start: string,
  predicate: (location: WorkspaceIndexLocation) => boolean,
): WorkspaceIndexLocation | null {
  let current = resolve(start);

  while (true) {
    const location = workspaceIndexLocation(current);
    if (predicate(location)) {
      return location;
    }

    const parent = dirname(current);
    if (parent === current) {
      return null;
    }

    current = parent;
  }
}

export function hasWorkspaceManifest(
  location: WorkspaceIndexLocation,
): boolean {
  return existsSync(location.manifestPath);
}

export function hasWorkspaceIndex(location: WorkspaceIndexLocation): boolean {
  return (
    hasWorkspaceManifest(location) &&
    existsSync(location.filesPath) &&
    existsSync(location.indexPath)
  );
}
