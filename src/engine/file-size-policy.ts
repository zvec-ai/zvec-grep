import type { FileKind } from "./types.js";

export const DEFAULT_MAX_CODE_FILE_SIZE_BYTES = 1 * 1024 * 1024;
export const DEFAULT_MAX_TEXT_FILE_SIZE_BYTES = 256 * 1024 * 1024;
export const DEFAULT_MAX_DATA_FILE_SIZE_BYTES = 16 * 1024 * 1024;
export const DEFAULT_MAX_IMAGE_FILE_SIZE_BYTES = 10 * 1024 * 1024;

export function resolveMaxFileSizeBytes(
  kind: FileKind,
  explicitMaxFileSizeBytes?: number,
): number {
  if (explicitMaxFileSizeBytes !== undefined) {
    return explicitMaxFileSizeBytes;
  }

  switch (kind) {
    case "code":
      return DEFAULT_MAX_CODE_FILE_SIZE_BYTES;
    case "text":
      return DEFAULT_MAX_TEXT_FILE_SIZE_BYTES;
    case "data":
      return DEFAULT_MAX_DATA_FILE_SIZE_BYTES;
    case "image":
      return DEFAULT_MAX_IMAGE_FILE_SIZE_BYTES;
  }
}
