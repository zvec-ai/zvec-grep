import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { deleteWorkspaceManifest, workspaceManifestPath } from "../manifest.js";
import {
  deleteWorkspaceIndexStorage,
  hasWorkspaceIndexStorage,
} from "../storage/index.js";
import { workspaceIndexPath } from "../storage/layout.js";

export const ZVEC_GREP_DIR = ".zvec-grep";
export type WorkspaceIndexLocation = {
  root: string;
  home: string;
  manifestPath: string;
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
    indexPath: workspaceIndexPath(home),
  };
}

export function resetWorkspaceIndex(location: WorkspaceIndexLocation): void {
  deleteWorkspaceManifest(location.home);
  deleteWorkspaceIndexStorage(location.home);
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
    hasWorkspaceManifest(location) && hasWorkspaceIndexStorage(location.home)
  );
}
