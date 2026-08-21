import type { CodeEntityModifier, CodeSymbolType } from "../../types.js";
import type { StructuredCodeFormat } from "../../code-formats.js";
import type { TSNode } from "./tree-sitter/nodes.js";
import { C_ADAPTER } from "./languages/c.js";
import { CPP_ADAPTER } from "./languages/cpp.js";
import { GO_ADAPTER } from "./languages/go.js";
import { JAVA_ADAPTER } from "./languages/java.js";
import { JAVASCRIPT_ADAPTER } from "./languages/javascript.js";
import { PYTHON_ADAPTER } from "./languages/python.js";
import { RUST_ADAPTER } from "./languages/rust.js";
import { TYPESCRIPT_ADAPTER } from "./languages/typescript.js";
import { extractCallableArity } from "./callable-shape.js";
import {
  extractCallResolutionFacts,
  type CallResolutionFact,
} from "./call-resolution-facts.js";

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
  extractArity?(node: TSNode): number | undefined;
  extractCallResolutionFacts?(
    node: TSNode,
  ): ReadonlyMap<string, CallResolutionFact>;
  extractDoc?(node: TSNode): string | undefined;
  extractModifiers?(node: TSNode): readonly CodeEntityModifier[];
};

const ADAPTERS = {
  c: withCallableShape(C_ADAPTER),
  cpp: withCallableShape(CPP_ADAPTER),
  go: withCallableShape(GO_ADAPTER),
  java: withCallableShape(JAVA_ADAPTER),
  javascript: withCallableShape(JAVASCRIPT_ADAPTER),
  jsx: withCallableShape(JAVASCRIPT_ADAPTER),
  python: withCallableShape(PYTHON_ADAPTER),
  rust: withCallableShape(RUST_ADAPTER),
  tsx: withCallableShape(TYPESCRIPT_ADAPTER),
  typescript: withCallableShape(TYPESCRIPT_ADAPTER),
} satisfies Record<StructuredCodeFormat, LanguageAdapter>;

function withCallableShape(adapter: LanguageAdapter): LanguageAdapter {
  return {
    ...adapter,
    extractArity: (node) => extractCallableArity(node, adapter.format),
    extractCallResolutionFacts: (node) =>
      extractCallResolutionFacts(node, adapter),
  };
}

export function resolveAdapter(format: string): LanguageAdapter | null {
  return format in ADAPTERS ? ADAPTERS[format as StructuredCodeFormat] : null;
}
