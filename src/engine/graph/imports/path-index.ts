import { dirname, relative, resolve, sep } from "node:path";
import type { FileInfo } from "../../types.js";
import { normalizePath, toDisplayPath } from "../../utils/path.js";

export type IndexedFile = {
  id: string;
  absolutePath: string;
  relativePath: string;
  rootPath: string;
  format: string;
};

/** Lookup table for import path → fileId within an indexed collection. */
export class FilePathIndex {
  private readonly byAbsolute = new Map<string, IndexedFile>();
  /** rootPath\0relativePath → file */
  private readonly byRootRelative = new Map<string, IndexedFile>();
  private readonly byId = new Map<string, IndexedFile>();

  constructor(files: readonly FileInfo[] = []) {
    this.addFiles(files);
  }

  addFiles(files: readonly FileInfo[]): void {
    for (const file of files) {
      const entry: IndexedFile = {
        id: file.id,
        absolutePath: normalizePath(file.absolutePath),
        relativePath: toDisplayPath(file.relativePath),
        rootPath: normalizePath(file.rootPath),
        format: file.format,
      };
      this.byAbsolute.set(entry.absolutePath, entry);
      this.byRootRelative.set(
        `${entry.rootPath}\0${entry.relativePath}`,
        entry,
      );
      this.byId.set(entry.id, entry);
    }
  }

  getById(fileId: string): IndexedFile | undefined {
    return this.byId.get(fileId);
  }

  /** True if an absolute path (any extension variant already applied) is indexed. */
  hasAbsolute(absolutePath: string): boolean {
    return this.byAbsolute.has(normalizePath(absolutePath));
  }

  getByAbsolute(absolutePath: string): IndexedFile | undefined {
    return this.byAbsolute.get(normalizePath(absolutePath));
  }

  /**
   * Try candidate absolute paths (with extensions); return first indexed hit.
   */
  findAbsolute(candidates: readonly string[]): IndexedFile | undefined {
    for (const candidate of candidates) {
      const hit = this.getByAbsolute(candidate);
      if (hit) {
        return hit;
      }
    }
    return undefined;
  }

  dirOf(fileId: string): string | undefined {
    const file = this.byId.get(fileId);
    return file ? dirname(file.absolutePath) : undefined;
  }

  relativeToRoot(fileId: string, absolutePath: string): string | undefined {
    const file = this.byId.get(fileId);
    if (!file) {
      return undefined;
    }
    return toDisplayPath(relative(file.rootPath, normalizePath(absolutePath)));
  }

  /** All indexed absolute paths (for debugging / tests). */
  absolutePaths(): string[] {
    return [...this.byAbsolute.keys()];
  }
}

export function joinDisplay(...parts: string[]): string {
  return parts.join("/").replace(/\\/g, "/").replace(/\/+/g, "/");
}

export function toPosix(path: string): string {
  return path.split(sep).join("/");
}

export function resolveAbsolute(fromDir: string, spec: string): string {
  return normalizePath(resolve(fromDir, spec));
}
