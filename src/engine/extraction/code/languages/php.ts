import type { CodeEntityModifier } from "../../../types.js";
import type { TSNode } from "../tree-sitter/nodes.js";
import type { LanguageAdapter } from "../adapter.js";
import {
  extractGenericSignature,
  extractPrecedingDoc,
} from "../families/metadata.js";

const MULTI_ELEMENT_TYPES = new Set([
  "const_declaration",
  "property_declaration",
]);

const ELEMENT_TYPES = new Set(["const_element", "property_element"]);

const VALUE_TYPES = new Set([...ELEMENT_TYPES, "property_promotion_parameter"]);

export const PHP_ADAPTER: LanguageAdapter = {
  format: "php",
  entityTypes: new Set([
    "class_declaration",
    "const_declaration",
    "enum_case",
    "enum_declaration",
    "function_definition",
    "interface_declaration",
    "method_declaration",
    "namespace_definition",
    "property_declaration",
    "trait_declaration",
  ]),
  scopeTypes: new Set([
    "class_declaration",
    "enum_declaration",
    "interface_declaration",
    "namespace_definition",
    "trait_declaration",
  ]),
  resolveEntities(node) {
    if (MULTI_ELEMENT_TYPES.has(node.type)) {
      const elements = node.namedChildren.filter((child) =>
        ELEMENT_TYPES.has(child.type),
      );

      return elements.length > 0 ? elements : [node];
    }

    return [node, ...promotedProperties(node)];
  },
  extractName(node) {
    if (!VALUE_TYPES.has(node.type)) {
      return node.childForFieldName("name")?.text;
    }

    const variable = node.namedChildren.find(
      (child) => child.type === "variable_name",
    );

    return (variable ?? node).namedChildren.find(
      (child) => child.type === "name",
    )?.text;
  },
  classifyNode(node) {
    return VALUE_TYPES.has(node.type) || node.type === "enum_case"
      ? "value"
      : undefined;
  },
  extractSignature(node) {
    const declaration = declarationOf(node);

    return declaration.childForFieldName("body")
      ? extractGenericSignature(declaration)
      : compactSignature(declaration);
  },
  extractDoc(node) {
    return extractPrecedingDoc(declarationOf(node));
  },
  extractModifiers(node) {
    const modifiers = new Set<CodeEntityModifier>();

    for (const child of declarationOf(node).namedChildren) {
      if (child.type === "static_modifier") {
        modifiers.add("static");
        continue;
      }

      if (child.type === "visibility_modifier") {
        modifiers.add(visibilityOf(child));
      }
    }

    return [...modifiers];
  },
};

// Declarations without a body carry no line structure worth keeping, and the
// shared helper would truncate a wrapped one to its first line. Property hooks
// are cut instead, so a hook body never reaches the signature.
function compactSignature(node: TSNode): string | undefined {
  const hooks = node.namedChildren.find(
    (child) => child.type === "property_hook_list",
  );
  const text = hooks
    ? node.text.slice(0, hooks.startIndex - node.startIndex)
    : node.text;
  const normalized = text
    .replace(/\s+/g, " ")
    .replace(/\s*[{;]\s*$/, "")
    .trim();

  return normalized.length > 0 ? normalized : undefined;
}

// Promoted parameters declare properties, so they are indexed beside the
// constructor rather than through it: the walk never descends into an entity.
function promotedProperties(node: TSNode): readonly TSNode[] {
  return (
    node
      .childForFieldName("parameters")
      ?.namedChildren.filter(
        (child) => child.type === "property_promotion_parameter",
      ) ?? []
  );
}

function declarationOf(node: TSNode): TSNode {
  return ELEMENT_TYPES.has(node.type) ? (node.parent ?? node) : node;
}

// PHP 8.4 asymmetric visibility reads as "private(set)"; the set half is dropped.
function visibilityOf(node: TSNode): CodeEntityModifier {
  return node.text.split("(")[0].trim() as CodeEntityModifier;
}
