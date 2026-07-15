import type { CodeEntityModifier } from "../../../types.js";
import type { TSNode } from "../../tree-sitter/nodes.js";
import type { LanguageAdapter } from "../adapter.js";
import {
  extractGenericSignature,
  extractPrecedingDoc,
} from "../families/metadata.js";

export const GO_ADAPTER: LanguageAdapter = {
  format: "go",
  entityTypes: new Set([
    "function_declaration",
    "method_spec",
    "method_declaration",
    "type_alias",
    "type_spec",
  ]),
  scopeTypes: new Set(["type_spec"]),
  extractName(node) {
    return node.childForFieldName("name")?.text;
  },
  shouldEnterScope(node) {
    if (node.type !== "type_spec") {
      return true;
    }

    const type = node.childForFieldName("type");

    return type?.type === "interface_type" || type?.type === "struct_type";
  },
  enterScopeNode(node) {
    return node.childForFieldName("type") ?? node;
  },
  scopeBreadcrumb(node, breadcrumb) {
    if (node.type !== "method_declaration") {
      return breadcrumb;
    }

    const receiverType = extractGoReceiverType(node);

    return receiverType ? [...breadcrumb, receiverType] : breadcrumb;
  },
  classifyNode(node) {
    if (node.type === "type_alias") {
      return "alias";
    }

    if (node.type === "type_spec") {
      const type = node.childForFieldName("type");
      if (type?.type === "interface_type") {
        return "interface";
      }
      if (type?.type === "struct_type") {
        return "class";
      }
      return "alias";
    }

    if (node.type === "method_spec") {
      return "function";
    }

    return undefined;
  },
  extractSignature: extractGenericSignature,
  extractDoc: extractPrecedingDoc,
  extractModifiers(node) {
    const name = node.childForFieldName("name")?.text;
    const modifiers: CodeEntityModifier[] = [];

    if (name && /^[A-Z]/.test(name)) {
      modifiers.push("exported");
    }

    return modifiers;
  },
};

function extractGoReceiverType(node: TSNode): string | undefined {
  const receiver = node.childForFieldName("receiver");
  if (!receiver) {
    return undefined;
  }

  const match = receiver.text.match(/\*?\s*([A-Za-z_][A-Za-z0-9_]*)\s*\)/);

  return match?.[1];
}
