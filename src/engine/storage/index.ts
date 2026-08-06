import type {
  CodeSymbolType,
  Entity,
  EntityFragment,
  FileInfo,
} from "../types.js";

export type StoredEntity = {
  entity: Entity;
  file: FileInfo;
};

export type StoredEntityFragment = {
  fragment: EntityFragment;
  file: FileInfo;
};

export type StorageSearchPath = "fts" | "vector";

export type StorageSearchFilter = {
  fileIds?: readonly string[];
  groupIds?: readonly string[];
  symbolNames?: readonly string[];
  symbolTypes?: readonly CodeSymbolType[];
};

export type StorageSearchHit = StoredEntityFragment & {
  path: StorageSearchPath;
  score: number;
};

export type FileIndexDiagnostics = {
  truncatedFragmentCount?: number;
};

export interface WorkspaceIndexStorage {
  getFileById(fileId: string): FileInfo | null;
  getFileByPath(absolutePath: string): FileInfo | null;
  listFilesByPathPrefix(absolutePath: string): FileInfo[];
  listFiles(): FileInfo[];
  listEntitiesByFile(
    fileId: string,
    options?: { limit?: number; offset?: number },
  ): StoredEntity[];
  getEntity(
    entityId: string,
    options?: { includeVector?: boolean },
  ): (StoredEntity & { vector?: number[] }) | null;
  upsertFile(
    file: FileInfo,
    fragments: readonly EntityFragment[],
    vectors: readonly number[][],
    diagnostics?: FileIndexDiagnostics,
  ): void;
  markFileFailed(file: FileInfo, error: string): void;
  deleteFile(fileId: string): void;
  searchFts(
    query: string,
    limit: number,
    filter?: StorageSearchFilter,
  ): StorageSearchHit[];
  searchVector(
    vector: readonly number[],
    limit: number,
    filter?: StorageSearchFilter,
  ): StorageSearchHit[];
  optimize(): Promise<void>;
  close(): void;
}

export { ZvecWorkspaceIndexStorage } from "./zvec.js";
