import { dirname } from "node:path";
import {
  resolveAbsolute,
  type FilePathIndex,
  type IndexedFile,
} from "./path-index.js";

const EXTENSION_RESOLUTION: Record<string, readonly string[]> = {
  typescript: [
    ".ts",
    ".tsx",
    ".d.ts",
    ".js",
    ".jsx",
    "/index.ts",
    "/index.tsx",
    "/index.js",
  ],
  tsx: [
    ".tsx",
    ".ts",
    ".d.ts",
    ".js",
    ".jsx",
    "/index.tsx",
    "/index.ts",
    "/index.js",
  ],
  javascript: [".js", ".jsx", ".mjs", ".cjs", "/index.js", "/index.jsx"],
  jsx: [".jsx", ".js", "/index.jsx", "/index.js"],
  python: [".py", "/__init__.py"],
  c: [".h", ".c"],
  cpp: [".h", ".hpp", ".hxx", ".cpp", ".cc", ".cxx"],
};

const NODE_BUILTINS = new Set([
  "assert",
  "buffer",
  "child_process",
  "crypto",
  "events",
  "fs",
  "http",
  "https",
  "os",
  "path",
  "stream",
  "url",
  "util",
  "node:fs",
  "node:path",
  "node:os",
  "node:util",
  "node:http",
  "node:crypto",
]);

const PYTHON_STDLIB = new Set([
  "os",
  "sys",
  "re",
  "json",
  "typing",
  "collections",
  "pathlib",
  "asyncio",
  "functools",
  "itertools",
  "datetime",
  "logging",
  "unittest",
  "pytest",
]);

const C_STD_HEADERS = new Set([
  "stdio.h",
  "stdlib.h",
  "string.h",
  "math.h",
  "stdint.h",
  "stddef.h",
  "stdbool.h",
  "assert.h",
  "ctype.h",
  "errno.h",
  "time.h",
  "unistd.h",
]);

export type ImportResolveResult =
  | { status: "resolved"; fileId: string; absolutePath: string }
  | { status: "external" }
  | { status: "failed" };

/**
 * Resolve an import/include specifier to an indexed file id.
 * v1: relative paths + language extension table; bare/stdlib → external.
 */
export function resolveImportPath(
  spec: string,
  fromFileId: string,
  language: string,
  index: FilePathIndex,
): ImportResolveResult {
  const trimmed = spec.trim();
  if (!trimmed) {
    return { status: "failed" };
  }

  if (isExternalImportSpec(trimmed, language)) {
    return { status: "external" };
  }

  const from = index.getById(fromFileId);
  if (!from) {
    return { status: "failed" };
  }

  const fromDir = dirname(from.absolutePath);
  const extensions = EXTENSION_RESOLUTION[language] ?? [];

  if (language === "python" && trimmed.startsWith(".")) {
    const hit = resolvePythonRelative(trimmed, fromDir, index, extensions);
    return hit
      ? { status: "resolved", fileId: hit.id, absolutePath: hit.absolutePath }
      : { status: "failed" };
  }

  if (trimmed.startsWith(".")) {
    const base = resolveAbsolute(fromDir, trimmed);
    const hit = tryExtensions(base, index, extensions);
    return hit
      ? { status: "resolved", fileId: hit.id, absolutePath: hit.absolutePath }
      : { status: "failed" };
  }

  // C/C++ quoted include without ./ — try relative to including file.
  if ((language === "c" || language === "cpp") && !trimmed.includes("://")) {
    const base = resolveAbsolute(fromDir, trimmed);
    const hit = tryExtensions(base, index, extensions);
    if (hit) {
      return {
        status: "resolved",
        fileId: hit.id,
        absolutePath: hit.absolutePath,
      };
    }
  }

  // Absolute-ish project paths like src/foo (no leading .)
  if (
    trimmed.startsWith("src/") ||
    trimmed.startsWith("@/") ||
    trimmed.startsWith("~/")
  ) {
    const rewritten = trimmed.replace(/^@\//, "src/").replace(/^~\//, "src/");
    const base = resolveAbsolute(from.rootPath, rewritten);
    const hit = tryExtensions(base, index, extensions);
    if (hit) {
      return {
        status: "resolved",
        fileId: hit.id,
        absolutePath: hit.absolutePath,
      };
    }
  }

  return { status: "failed" };
}

export function isExternalImportSpec(spec: string, language: string): boolean {
  if (spec.startsWith(".")) {
    return false;
  }

  if (
    language === "typescript" ||
    language === "tsx" ||
    language === "javascript" ||
    language === "jsx"
  ) {
    if (
      NODE_BUILTINS.has(spec) ||
      NODE_BUILTINS.has(spec.split("/")[0] ?? "")
    ) {
      return true;
    }
    if (
      spec.startsWith("@/") ||
      spec.startsWith("~/") ||
      spec.startsWith("src/")
    ) {
      return false;
    }
    // Bare npm / scoped packages
    return true;
  }

  if (language === "python") {
    const root = spec.split(".")[0] ?? spec;
    return PYTHON_STDLIB.has(root);
  }

  if (language === "c" || language === "cpp") {
    return C_STD_HEADERS.has(spec);
  }

  return false;
}

function resolvePythonRelative(
  spec: string,
  fromDir: string,
  index: FilePathIndex,
  extensions: readonly string[],
): IndexedFile | undefined {
  const dots = spec.length - spec.replace(/^\.+/, "").length;
  const up = "../".repeat(Math.max(0, dots - 1));
  const rest = spec.slice(dots).replace(/\./g, "/");
  const base = resolveAbsolute(fromDir, up + rest);
  return tryExtensions(base, index, extensions);
}

function tryExtensions(
  baseAbsolute: string,
  index: FilePathIndex,
  extensions: readonly string[],
): IndexedFile | undefined {
  const candidates = [
    baseAbsolute,
    ...extensions.map((ext) =>
      ext.startsWith("/") ? `${baseAbsolute}${ext}` : `${baseAbsolute}${ext}`,
    ),
  ];
  return index.findAbsolute(candidates);
}
