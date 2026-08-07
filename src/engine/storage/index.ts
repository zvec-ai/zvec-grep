import type {
  CodeSymbolType,
  Entity,
  EntityFragment,
  FileInfo,
  WorkspaceIndexEmbeddingSchema,
} from "../types.js";

type StoredEntity = {
  entity: Entity;
  file: FileInfo;
};

type IndexedFragment = {
  fragment: EntityFragment;
  vector: readonly number[];
};

type FileIndexDiagnostics = {
  truncatedFragmentCount?: number;
};

type StorageSearchFilter = {
  fileIds?: readonly string[];
  groupIds?: readonly string[];
  symbolNames?: readonly string[];
  symbolTypes?: readonly CodeSymbolType[];
};

type StorageSearchHit = {
  fragment: EntityFragment;
  file: FileInfo;
  path: "fts" | "vector";
  score: number;
};

export type WorkspaceIndexStorageOptions =
  | {
      storagePath: string;
      readOnly: true;
    }
  | {
      storagePath: string;
      readOnly: false;
      embedding: WorkspaceIndexEmbeddingSchema;
    };

export interface WorkspaceIndexStorage {
  readonly readOnly: boolean;
  getFileByPath(absolutePath: string): FileInfo | null;
  listFilesByPathPrefix(absolutePath: string): FileInfo[];
  listFiles(): FileInfo[];
  listEntitiesByFile(
    fileId: string,
    options?: { limit?: number; offset?: number },
  ): StoredEntity[];
  getEntity(entityId: string): StoredEntity | null;
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
  replaceFile(
    file: FileInfo,
    entries: readonly IndexedFragment[],
    diagnostics?: FileIndexDiagnostics,
  ): void;
  markFileFailed(file: FileInfo, error: string): void;
  deleteFile(fileId: string): void;
  finalizeWrites(): Promise<void>;
  close(): void;
}

export { createWorkspaceIndexStorage } from "./zvec.js";
export {
  deleteWorkspaceIndexStorage,
  hasWorkspaceIndexStorage,
} from "./layout.js";
