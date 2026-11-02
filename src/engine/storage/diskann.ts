import { createRequire } from "node:module";
import type { WorkspaceIndexEmbeddingSchema } from "../types.js";

export type DiskannIndex = {
  upsert(entries: { documentId: string; vector: number[] }[]): void;
  deleteDocuments(ids: string[]): void;
  search(
    vector: number[],
    limit: number,
    allowed?: string[],
  ): { documentId: string; score: number }[];
  close(): void;
};

export function openDiskann(
  path: string,
  embedding: WorkspaceIndexEmbeddingSchema | undefined,
  readOnly: boolean,
): DiskannIndex {
  const require = createRequire(import.meta.url);
  const binding = require(
    `../../../native/zg-diskann.${process.platform}-${process.arch}.node`,
  ) as {
    DiskannIndex: new (
      path: string,
      dimension: number,
      metric: string,
      readOnly: boolean,
    ) => DiskannIndex;
  };
  return new binding.DiskannIndex(
    path,
    embedding?.dimension ?? 0,
    embedding?.metric ?? "",
    readOnly,
  );
}
