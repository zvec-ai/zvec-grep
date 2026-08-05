import type { TSNode } from "../tree-sitter/nodes.js";
import type { LanguageAdapter } from "../adapter.js";
import {
  extractCommonModifiers,
  extractGenericSignature,
  extractPrecedingDoc,
} from "./metadata.js";

export type NameFieldAdapterOptions = Pick<
  LanguageAdapter,
  "shouldIndexEntity"
>;

export function createNameFieldAdapter(
  format: string,
  entityTypes: readonly string[],
  scopeTypes: readonly string[] = [],
  options: NameFieldAdapterOptions = {},
): LanguageAdapter {
  return {
    format,
    entityTypes: new Set(entityTypes),
    scopeTypes: new Set(scopeTypes),
    ...options,
    extractName(node) {
      return (
        node.childForFieldName("name")?.text ??
        findNamedIdentifierChild(node)?.text
      );
    },
    extractSignature: extractGenericSignature,
    extractDoc: extractPrecedingDoc,
    extractModifiers: extractCommonModifiers,
  };
}

function findNamedIdentifierChild(node: TSNode): TSNode | undefined {
  return node.namedChildren.find(
    (child) =>
      child.type === "identifier" ||
      child.type === "property_identifier" ||
      child.type === "type_identifier",
  );
}
