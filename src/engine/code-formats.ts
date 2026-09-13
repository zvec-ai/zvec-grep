export const STRUCTURED_CODE_FORMATS = [
  "c",
  "cpp",
  "go",
  "java",
  "javascript",
  "jsx",
  "php",
  "python",
  "rust",
  "tsx",
  "typescript",
] as const;

export type StructuredCodeFormat = (typeof STRUCTURED_CODE_FORMATS)[number];

export const COMPONENT_CODE_FORMATS = ["vue", "svelte"] as const;
