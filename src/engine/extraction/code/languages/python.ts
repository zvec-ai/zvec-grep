import type { CodeEntityModifier } from "../../../types.js";
import type { TSNode } from "../tree-sitter/nodes.js";
import type { LanguageAdapter } from "../adapter.js";
import {
  extractCommonModifiers,
  extractGenericSignature,
  extractPrecedingDoc,
} from "../families/metadata.js";

export const PYTHON_ADAPTER: LanguageAdapter = {
  format: "python",
  entityTypes: new Set([
    "class_definition",
    "decorated_definition",
    "function_definition",
  ]),
  scopeTypes: new Set(["class_definition", "decorated_definition"]),
  extractName(node) {
    if (node.type === "decorated_definition") {
      const inner = node.namedChildren.find(
        (child) =>
          child.type === "function_definition" ||
          child.type === "class_definition",
      );
      return inner ? this.extractName(inner) : undefined;
    }

    return node.childForFieldName("name")?.text;
  },
  shouldEnterScope(node) {
    if (node.type !== "decorated_definition") {
      return true;
    }

    return node.namedChildren.some(
      (child) => child.type === "class_definition",
    );
  },
  enterScopeNode(node) {
    if (node.type !== "decorated_definition") {
      return node;
    }

    return (
      node.namedChildren.find((child) => child.type === "class_definition") ??
      node
    );
  },
  extractSignature(node) {
    return extractGenericSignature(innerPythonDefinition(node) ?? node);
  },
  extractDoc: extractPrecedingDoc,
  extractModifiers(node) {
    const modifiers = new Set<CodeEntityModifier>(extractCommonModifiers(node));

    if (/^\s*async\s+def\b/m.test(node.text)) {
      modifiers.add("async");
    }

    if (/^\s*@staticmethod\b/m.test(node.text)) {
      modifiers.add("static");
    }

    return [...modifiers];
  },
};

function innerPythonDefinition(node: TSNode): TSNode | undefined {
  if (node.type !== "decorated_definition") {
    return undefined;
  }

  return node.namedChildren.find(
    (child) =>
      child.type === "function_definition" || child.type === "class_definition",
  );
}
