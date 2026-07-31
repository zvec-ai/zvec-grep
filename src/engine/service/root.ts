import { existsSync, rmSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

export const ZVEC_GREP_DIR = ".zvec-grep";
export const ANONYMOUS_COLLECTION_NAME = "__anonymous__";
export const ANONYMOUS_COLLECTION_DIR = "index";

const COLLECTIONS_ZVEC = "collections.zvec";
const FILES_ZVEC = "files.zvec";
const ENTITIES_ZVEC = "index.zvec";

export type AnonymousIndexLocation = {
  root: string;
  home: string;
  collectionName: typeof ANONYMOUS_COLLECTION_NAME;
  collectionPath: string;
};

export function resolveZvecGrepRoot(root: string | undefined): string {
  return resolve(root ?? process.cwd());
}

export function anonymousHome(root: string): string {
  return join(resolve(root), ZVEC_GREP_DIR);
}

export function anonymousCollectionPath(root: string): string {
  return join(anonymousHome(root), ANONYMOUS_COLLECTION_DIR);
}

export function anonymousIndexLocation(root: string): AnonymousIndexLocation {
  const resolvedRoot = resolve(root);

  return {
    root: resolvedRoot,
    home: anonymousHome(resolvedRoot),
    collectionName: ANONYMOUS_COLLECTION_NAME,
    collectionPath: anonymousCollectionPath(resolvedRoot),
  };
}

export function resetAnonymousIndexStorage(
  location: AnonymousIndexLocation,
): void {
  if (dirname(location.collectionPath) !== location.home) {
    throw new Error("Anonymous index path must be inside its workspace home");
  }
  for (const target of [
    join(location.home, COLLECTIONS_ZVEC),
    join(location.home, FILES_ZVEC),
    location.collectionPath,
  ]) {
    rmSync(target, { recursive: true, force: true });
  }
}

export function findNearestAnonymousIndex(
  start: string,
): AnonymousIndexLocation | null {
  return findNearestAnonymousLocation(start, hasAnonymousIndex);
}

export function findNearestAnonymousWorkspace(
  start: string,
): AnonymousIndexLocation | null {
  return findNearestAnonymousLocation(start, hasAnonymousMetadata);
}

function findNearestAnonymousLocation(
  start: string,
  predicate: (location: AnonymousIndexLocation) => boolean,
): AnonymousIndexLocation | null {
  let current = resolve(start);

  while (true) {
    const location = anonymousIndexLocation(current);
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

export function hasAnonymousMetadata(
  location: AnonymousIndexLocation,
): boolean {
  return existsSync(join(location.home, COLLECTIONS_ZVEC));
}

export function hasAnonymousIndex(location: AnonymousIndexLocation): boolean {
  return (
    existsSync(join(location.home, COLLECTIONS_ZVEC)) &&
    existsSync(join(location.collectionPath, ENTITIES_ZVEC))
  );
}
