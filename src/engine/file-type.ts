import { basename, extname } from "node:path";
import type { FileFormat, FileKind, ImageFormat } from "./types.js";

const CODE_FORMATS: Record<string, string> = {
  ".c": "c",
  ".cc": "cpp",
  ".cpp": "cpp",
  ".cxx": "cpp",
  ".h": "cpp",
  ".hpp": "cpp",
  ".go": "go",
  ".java": "java",
  ".js": "javascript",
  ".jsx": "jsx",
  ".mjs": "javascript",
  ".cjs": "javascript",
  ".ts": "typescript",
  ".tsx": "tsx",
  ".py": "python",
  ".rs": "rust",
  ".rb": "ruby",
  ".php": "php",
  ".swift": "swift",
  ".kt": "kotlin",
  ".kts": "kotlin",
  ".cs": "csharp",
  ".scala": "scala",
  ".sh": "bash",
  ".bash": "bash",
  ".zsh": "bash",
  ".sql": "sql",
  ".css": "css",
  ".scss": "scss",
  ".less": "less",
  ".vue": "vue",
  ".svelte": "svelte",
};

const DATA_FORMATS: Record<string, string> = {
  ".csv": "csv",
  ".json": "json",
  ".jsonc": "json",
  ".toml": "toml",
  ".yaml": "yaml",
  ".yml": "yaml",
};

const TEXT_FORMATS: Record<string, string> = {
  ".md": "markdown",
  ".mdx": "markdown",
  ".rst": "rst",
  ".txt": "text",
  ".html": "html",
  ".htm": "html",
  ".xml": "xml",
};

const IMAGE_FORMATS: Record<string, ImageFormat> = {
  ".gif": "gif",
  ".jpeg": "jpeg",
  ".jpg": "jpeg",
  ".png": "png",
  ".webp": "webp",
};

const NAMED_FILE_FORMATS: Record<
  string,
  { kind: FileKind; format: FileFormat }
> = {
  Dockerfile: { kind: "code", format: "dockerfile" },
  Makefile: { kind: "code", format: "makefile" },
};

const FILE_FORMATS_BY_KIND = {
  code: CODE_FORMATS,
  data: DATA_FORMATS,
  text: TEXT_FORMATS,
  image: IMAGE_FORMATS,
} satisfies Record<FileKind, Record<string, string>>;

const BINARY_EXTENSION_GROUPS = [
  {
    label: "Archives",
    extensions: [".zip", ".tar", ".gz", ".bz2", ".xz", ".7z", ".rar"],
  },
  {
    label: "Compiled",
    extensions: [
      ".exe",
      ".dll",
      ".dylib",
      ".so",
      ".a",
      ".o",
      ".obj",
      ".wasm",
      ".class",
      ".jar",
    ],
  },
  {
    label: "Documents",
    extensions: [".pdf", ".doc", ".docx", ".ppt", ".pptx", ".xls", ".xlsx"],
  },
  {
    label: "Media",
    extensions: [".mp3", ".mp4", ".mov", ".avi", ".mkv"],
  },
  { label: "Databases", extensions: [".db", ".sqlite"] },
] as const;

const BINARY_EXTENSIONS = new Set<string>(
  BINARY_EXTENSION_GROUPS.flatMap((group) => group.extensions),
);

export type RecognizedFileType = {
  kind: FileKind;
  format: FileFormat;
  patterns: string[];
};

export function listRecognizedFileTypes(): RecognizedFileType[] {
  const recognized = new Map<string, RecognizedFileType>();
  const append = (
    kind: FileKind,
    format: FileFormat,
    pattern: string,
  ): void => {
    const key = `${kind}:${format}`;
    const existing = recognized.get(key);
    if (existing) {
      existing.patterns.push(pattern);
      return;
    }
    recognized.set(key, { kind, format, patterns: [pattern] });
  };

  for (const kind of Object.keys(FILE_FORMATS_BY_KIND) as FileKind[]) {
    for (const [extension, format] of Object.entries(
      FILE_FORMATS_BY_KIND[kind],
    )) {
      append(kind, format, extension);
    }
  }
  for (const [name, type] of Object.entries(NAMED_FILE_FORMATS)) {
    append(type.kind, type.format, name);
  }

  return [...recognized.values()];
}

export function listKnownBinaryExtensionGroups(): {
  label: string;
  extensions: string[];
}[] {
  return BINARY_EXTENSION_GROUPS.map((group) => ({
    label: group.label,
    extensions: [...group.extensions],
  }));
}

export function detectFileType(
  path: string,
): { kind: FileKind; format: FileFormat } | null {
  const name = basename(path);
  const namedType = NAMED_FILE_FORMATS[name];
  if (namedType) {
    return { ...namedType };
  }

  const extension = extname(name).toLowerCase();

  if (extension in CODE_FORMATS) {
    return { kind: "code", format: CODE_FORMATS[extension] };
  }

  if (extension in DATA_FORMATS) {
    return { kind: "data", format: DATA_FORMATS[extension] };
  }

  if (extension in TEXT_FORMATS) {
    return { kind: "text", format: TEXT_FORMATS[extension] };
  }

  if (extension in IMAGE_FORMATS) {
    return { kind: "image", format: IMAGE_FORMATS[extension] };
  }

  if (BINARY_EXTENSIONS.has(extension)) {
    return null;
  }

  return { kind: "text", format: extension ? extension.slice(1) : "text" };
}
