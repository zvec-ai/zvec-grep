import type { StoredEntity } from "../storage/index.js";

/** Read-only entity lookup port shared by graph application use cases. */
export type GraphQueryStorage = {
  findSymbolsByName(name: string, limit: number): StoredEntity[];
  findSymbolsByQuery?(query: string, limit: number): StoredEntity[];
  getEntity(entityId: string): StoredEntity | null;
};
