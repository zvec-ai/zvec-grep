import { createRequire } from "node:module";
import { join } from "node:path";
import Parser from "web-tree-sitter";

const require = createRequire(import.meta.url);

const LANGUAGE_WASM_MAP: Record<string, string> = {
  c: "c",
  cpp: "cpp",
  go: "go",
  java: "java",
  javascript: "javascript",
  jsx: "javascript",
  python: "python",
  rust: "rust",
  tsx: "tsx",
  typescript: "typescript",
};

let parserReady = false;
const grammarCache = new Map<string, Parser.Language>();

export async function ensureParser(): Promise<void> {
  if (!parserReady) {
    await Parser.init();
    parserReady = true;
  }
}

export async function loadGrammar(
  format: string,
): Promise<Parser.Language | null> {
  const cached = grammarCache.get(format);
  if (cached) {
    return cached;
  }

  const wasmName = LANGUAGE_WASM_MAP[format];
  if (!wasmName) {
    return null;
  }

  await ensureParser();

  try {
    const wasmsDir = join(
      require.resolve("tree-sitter-wasms/package.json"),
      "..",
      "out",
    );
    const wasmPath = join(wasmsDir, `tree-sitter-${wasmName}.wasm`);
    const grammar = await Parser.Language.load(wasmPath);
    grammarCache.set(format, grammar);

    return grammar;
  } catch {
    return null;
  }
}

export function hasGrammar(format: string): boolean {
  return format in LANGUAGE_WASM_MAP;
}
