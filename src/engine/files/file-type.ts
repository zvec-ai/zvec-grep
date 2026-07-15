import { basename, extname } from "node:path";
import type { FileFormat, FileKind, ImageFormat } from "../types.js";

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

const BINARY_EXTENSIONS = new Set([
  ".zip",
  ".tar",
  ".gz",
  ".bz2",
  ".xz",
  ".7z",
  ".rar",
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
  ".pdf",
  ".doc",
  ".docx",
  ".ppt",
  ".pptx",
  ".xls",
  ".xlsx",
  ".mp3",
  ".mp4",
  ".mov",
  ".avi",
  ".mkv",
  ".db",
  ".sqlite",
]);

export function detectFileType(
  path: string,
): { kind: FileKind; format: FileFormat } | null {
  const name = basename(path);

  if (name === "Dockerfile") {
    return { kind: "code", format: "dockerfile" };
  }

  if (name === "Makefile") {
    return { kind: "code", format: "makefile" };
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
