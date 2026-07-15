import type { CodeSymbolType } from "../../../types.js";
import { findIdentifierLeaf, type TSNode } from "../../tree-sitter/nodes.js";
import type { LanguageAdapter } from "../adapter.js";
import {
  extractCommonModifiers,
  extractGenericSignature,
  extractPrecedingDoc,
} from "./metadata.js";

const C_FAMILY_FUNCTION_TYPES = new Set([
  "declaration",
  "field_declaration",
  "function_definition",
  "macro_type_specifier",
]);

const C_FAMILY_FUNCTION_DECLARATION_TYPES = new Set([
  "declaration",
  "field_declaration",
]);

export function createCFamilyAdapter(
  format: string,
  entityTypes: readonly string[],
  scopeTypes: readonly string[] = [],
): LanguageAdapter {
  return {
    format,
    entityTypes: new Set(entityTypes),
    scopeTypes: new Set(scopeTypes),
    shouldIndexEntity(node) {
      if (!C_FAMILY_FUNCTION_DECLARATION_TYPES.has(node.type)) {
        if (node.type === "macro_type_specifier") {
          return extractCFunctionName(node.text) !== undefined;
        }

        return true;
      }

      return findDescendantByType(node, "function_declarator") !== null;
    },
    extractName(node) {
      if (C_FAMILY_FUNCTION_TYPES.has(node.type)) {
        const name = extractRawCFunctionName(node);

        return name ? lastQualifiedPart(name) : undefined;
      }

      const name = node.childForFieldName("name");
      if (name) {
        return findIdentifierLeaf(name)?.text ?? name.text;
      }

      if (node.type === "type_definition") {
        const declarator = node.childForFieldName("declarator");
        return declarator ? findIdentifierLeaf(declarator)?.text : undefined;
      }

      return undefined;
    },
    classifyNode: classifyCFamilyNode,
    scopeBreadcrumb: cFamilyScopeBreadcrumb,
    extractSignature: extractGenericSignature,
    extractDoc: extractPrecedingDoc,
    extractModifiers: extractCommonModifiers,
  };
}

function cFamilyScopeBreadcrumb(
  node: TSNode,
  breadcrumb: readonly string[],
): readonly string[] {
  if (!C_FAMILY_FUNCTION_TYPES.has(node.type)) {
    return breadcrumb;
  }

  const name = extractRawCFunctionName(node);
  const qualifier = name ? qualifierParts(name) : [];

  if (qualifier.length === 0) {
    return breadcrumb;
  }

  const parts =
    breadcrumb.length > 0 && breadcrumb[breadcrumb.length - 1] === qualifier[0]
      ? qualifier.slice(1)
      : qualifier;

  return [...breadcrumb, ...parts];
}

function classifyCFamilyNode(node: TSNode): CodeSymbolType | undefined {
  if (node.type === "type_definition") {
    return typedefWrapsClassLikeBody(node) ? "class" : "alias";
  }

  if (node.type === "alias_declaration") {
    return "alias";
  }

  if (
    node.type === "field_declaration" &&
    findDescendantByType(node, "function_declarator")
  ) {
    return "function";
  }

  return undefined;
}

function extractRawCFunctionName(node: TSNode): string | undefined {
  const declarator =
    node.childForFieldName("declarator") ??
    findDescendantByType(node, "function_declarator");
  const name = declarator ? findIdentifierLeaf(declarator)?.text : undefined;

  return isSimpleCIdentifier(name)
    ? name
    : extractCFunctionName(declarator?.text ?? node.text);
}

function qualifierParts(name: string): string[] {
  const parts = name.split("::").filter((part) => part.length > 0);

  return parts.length > 1 ? parts.slice(0, -1) : [];
}

function lastQualifiedPart(name: string): string {
  return (
    name
      .split("::")
      .filter((part) => part.length > 0)
      .at(-1) ?? name
  );
}

function typedefWrapsClassLikeBody(node: TSNode): boolean {
  return ["struct_specifier", "union_specifier", "enum_specifier"].some(
    (type) => {
      const child = findDescendantByType(node, type);
      return (
        child?.childForFieldName("body") !== null &&
        child?.childForFieldName("body") !== undefined
      );
    },
  );
}

function isSimpleCIdentifier(value: string | undefined): value is string {
  return (
    value !== undefined &&
    /^~?[A-Za-z_][A-Za-z0-9_]*(?:::[~A-Za-z_][A-Za-z0-9_]*)*$/.test(value)
  );
}

function extractCFunctionName(text: string): string | undefined {
  const matches = [...text.matchAll(/([~A-Za-z_][~A-Za-z0-9_:]*)\s*\(/g)];
  const [last] = matches.slice(-1);

  return last?.[1];
}

function findDescendantByType(node: TSNode, type: string): TSNode | null {
  if (node.type === type) {
    return node;
  }

  for (const child of node.namedChildren) {
    const found = findDescendantByType(child, type);
    if (found) {
      return found;
    }
  }

  return null;
}
