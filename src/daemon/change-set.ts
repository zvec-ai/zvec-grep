import { basename, dirname, isAbsolute, relative, sep } from "node:path";
import { normalizePath } from "../engine/utils/path.js";

export type ChangeKind = "created" | "changed" | "deleted";

export type ChangeSetSnapshot = {
  touchedFiles: string[];
  rescanDirectories: string[];
  deletedPrefixes: string[];
  forceFullReconcile: boolean;
};

export type ChangeSetOptions = {
  root?: string;
  maxChangedPaths?: number;
};

export class ChangeSet {
  private readonly touchedFiles = new Set<string>();
  private readonly rescanDirectories = new Set<string>();
  private readonly deletedPrefixes = new Set<string>();
  private forceFullReconcile = false;
  private readonly root?: string;
  private readonly maxChangedPaths: number;

  constructor(options: ChangeSetOptions = {}) {
    this.root = options.root ? normalizePath(options.root) : undefined;
    this.maxChangedPaths = options.maxChangedPaths ?? 1_000;
  }

  add(path: string, kind: ChangeKind, isDirectory = false): void {
    if (!isAbsolute(path)) {
      throw new Error("Changed paths must be absolute.");
    }
    if (this.forceFullReconcile && this.size > 0) {
      return;
    }
    const normalized = normalizePath(path);
    if (
      this.pathCoveredBy(this.rescanDirectories, normalized) ||
      this.pathCoveredBy(this.deletedPrefixes, normalized)
    ) {
      return;
    }
    if (basename(normalized) === ".gitignore") {
      this.rescanDirectories.add(dirname(normalized));
    } else if (kind === "deleted") {
      this.deletedPrefixes.add(normalized);
    } else if (isDirectory) {
      this.rescanDirectories.add(normalized);
    } else {
      this.touchedFiles.add(normalized);
    }
    if (this.size >= this.maxChangedPaths) {
      this.enforcePathBudget();
    }
  }

  requireFullReconcile(): void {
    this.forceFullReconcile = true;
  }

  merge(other: ChangeSetSnapshot): void {
    if (this.forceFullReconcile && this.size > 0) {
      return;
    }
    for (const path of other.touchedFiles) this.touchedFiles.add(path);
    for (const path of other.rescanDirectories)
      this.rescanDirectories.add(path);
    for (const path of other.deletedPrefixes) this.deletedPrefixes.add(path);
    this.forceFullReconcile ||= other.forceFullReconcile;
    if (!this.forceFullReconcile && this.size >= this.maxChangedPaths) {
      this.enforcePathBudget();
    }
  }

  snapshot(): ChangeSetSnapshot {
    this.collapsePaths();
    return {
      touchedFiles: [...this.touchedFiles].sort(),
      rescanDirectories: [...this.rescanDirectories].sort(),
      deletedPrefixes: [...this.deletedPrefixes].sort(),
      forceFullReconcile: this.forceFullReconcile,
    };
  }

  get size(): number {
    return (
      this.touchedFiles.size +
      this.rescanDirectories.size +
      this.deletedPrefixes.size
    );
  }

  get empty(): boolean {
    return this.size === 0 && !this.forceFullReconcile;
  }

  private collapsePaths(): void {
    collapseSet(this.rescanDirectories);
    collapseSet(this.deletedPrefixes);
    for (const file of this.touchedFiles) {
      if (
        hasAncestor(this.rescanDirectories, file) ||
        hasAncestor(this.deletedPrefixes, file)
      ) {
        this.touchedFiles.delete(file);
      }
    }
    for (const directory of this.rescanDirectories) {
      if (hasAncestor(this.deletedPrefixes, directory)) {
        this.rescanDirectories.delete(directory);
      }
    }
  }

  private enforcePathBudget(): void {
    this.collapsePaths();
    if (this.size < this.maxChangedPaths) {
      return;
    }
    if (!this.root) {
      this.forceFullReconcile = true;
      return;
    }
    // Exact watcher events are still trustworthy when the batch is large.
    // Widen their scope to parent directories to bound memory without turning
    // an exact update into a full reconciliation for possible missed events.
    const leafScopes = [...this.touchedFiles, ...this.deletedPrefixes];
    if (leafScopes.length > 0) {
      this.touchedFiles.clear();
      this.deletedPrefixes.clear();
      for (const path of leafScopes) {
        this.rescanDirectories.add(this.parentScope(path));
      }
      this.collapsePaths();
    }
    while (this.size >= this.maxChangedPaths) {
      const previousSize = this.size;
      const directories = [...this.rescanDirectories].map((path) =>
        this.parentScope(path),
      );
      this.rescanDirectories.clear();
      for (const directory of directories) {
        this.rescanDirectories.add(directory);
      }
      this.collapsePaths();
      if (this.size >= previousSize) {
        break;
      }
    }
  }

  private pathCoveredBy(paths: ReadonlySet<string>, path: string): boolean {
    return paths.has(path) || hasAncestor(paths, path);
  }

  private parentScope(path: string): string {
    if (path === this.root) {
      return path;
    }
    const parent = dirname(path);
    const fromRoot = relative(this.root!, parent);
    return isAbsolute(fromRoot) ||
      fromRoot === ".." ||
      fromRoot.startsWith(`..${sep}`)
      ? this.root!
      : parent;
  }
}

function collapseSet(paths: Set<string>): void {
  const sorted = [...paths].sort((left, right) => left.length - right.length);
  for (const path of sorted) {
    if (hasAncestor(paths, path)) {
      paths.delete(path);
    }
  }
}

function hasAncestor(paths: ReadonlySet<string>, target: string): boolean {
  let current = dirname(target);
  while (current !== target) {
    if (paths.has(current)) {
      return true;
    }
    const parent = dirname(current);
    if (parent === current) break;
    current = parent;
  }
  return false;
}
