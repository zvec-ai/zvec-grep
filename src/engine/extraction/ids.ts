import { sha256Text } from "../utils/hash.js";

export function makeEntityId(fileId: string, index: number): string {
  return sha256Text(`${fileId}\0${index}`);
}
