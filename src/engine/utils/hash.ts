import { createHash } from "node:crypto";

export function sha256Text(text: string): string {
  return createHash("sha256").update(text).digest("hex");
}

export function sha256Bytes(bytes: Uint8Array): string {
  return createHash("sha256").update(bytes).digest("hex");
}
