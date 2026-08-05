import type { CodeEntityModifier, CodeSymbolType } from "../../types.js";
import type { TSNode } from "./tree-sitter/nodes.js";
import { C_ADAPTER } from "./languages/c.js";
import { CPP_ADAPTER } from "./languages/cpp.js";
import { GO_ADAPTER } from "./languages/go.js";
import { JAVA_ADAPTER } from "./languages/java.js";
import { JAVASCRIPT_ADAPTER } from "./languages/javascript.js";
import { PYTHON_ADAPTER } from "./languages/python.js";
import { RUST_ADAPTER } from "./languages/rust.js";
import { TYPESCRIPT_ADAPTER } from "./languages/typescript.js";

export type LanguageAdapter = {
  format: string;
  entityTypes: ReadonlySet<string>;
  scopeTypes: ReadonlySet<string>;
  extractName(node: TSNode): string | undefined;
  shouldIndexEntity?(node: TSNode): boolean;
  shouldEnterScope?(node: TSNode): boolean;
  resolveEntities?(node: TSNode): readonly TSNode[];
  enterScopeNode?(node: TSNode): TSNode;
  resolveEntity?(node: TSNode): TSNode;
  scopeBreadcrumb?(
    node: TSNode,
    breadcrumb: readonly string[],
  ): readonly string[];
  classifyNode?(
    node: TSNode,
    breadcrumb: readonly string[],
  ): CodeSymbolType | undefined;
  extractSignature?(node: TSNode): string | undefined;
  extractDoc?(node: TSNode): string | undefined;
  extractModifiers?(node: TSNode): readonly CodeEntityModifier[];
};

const ADAPTERS: Record<string, LanguageAdapter> = {
  c: C_ADAPTER,
  cpp: CPP_ADAPTER,
  go: GO_ADAPTER,
  java: JAVA_ADAPTER,
  javascript: JAVASCRIPT_ADAPTER,
  jsx: JAVASCRIPT_ADAPTER,
  python: PYTHON_ADAPTER,
  rust: RUST_ADAPTER,
  tsx: TYPESCRIPT_ADAPTER,
  typescript: TYPESCRIPT_ADAPTER,
};

export function resolveAdapter(format: string): LanguageAdapter | null {
  return ADAPTERS[format] ?? null;
}
