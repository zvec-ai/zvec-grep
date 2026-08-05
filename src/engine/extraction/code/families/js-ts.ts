import type { CodeSymbolType } from "../../../types.js";
import type { TSNode } from "../tree-sitter/nodes.js";
import {
  closestAncestor,
  extractCommonModifiers,
  extractGenericSignature,
  extractPrecedingDoc,
} from "./metadata.js";

const JS_TS_FUNCTION_VALUE_DECLARATION_TYPES = new Set([
  "field_definition",
  "public_field_definition",
  "variable_declarator",
]);

const JS_TS_FUNCTION_VALUE_TYPES = new Set([
  "arrow_function",
  "function_expression",
]);

export function shouldIndexJavascriptTypescriptEntity(node: TSNode): boolean {
  if (node.type === "method_definition" && isObjectMember(node)) {
    return false;
  }

  if (node.type === "pair") {
    return (
      hasFunctionValue(node) && exportedObjectVariableName(node) !== undefined
    );
  }

  if (!JS_TS_FUNCTION_VALUE_DECLARATION_TYPES.has(node.type)) {
    return true;
  }

  if (
    node.type === "variable_declarator" &&
    exportedObjectFunctionEntities(node).length > 0
  ) {
    return true;
  }

  return hasFunctionValue(node);
}

export function hasJavascriptTypescriptFunctionValue(node: TSNode): boolean {
  return hasFunctionValue(node);
}

export function resolveJavascriptTypescriptEntities(
  node: TSNode,
): readonly TSNode[] {
  if (node.type !== "variable_declarator") {
    return [node];
  }

  const objectEntities = exportedObjectFunctionEntities(node);

  return objectEntities.length > 0 ? objectEntities : [node];
}

export function extractJavascriptTypescriptName(
  node: TSNode,
): string | undefined {
  if (node.type === "pair") {
    return node.childForFieldName("key")?.text.replace(/^['"`]|['"`]$/g, "");
  }

  return (
    node.childForFieldName("name")?.text ?? findNamedIdentifierChild(node)?.text
  );
}

export function javascriptTypescriptScopeBreadcrumb(
  node: TSNode,
  breadcrumb: readonly string[],
): readonly string[] {
  const objectName = exportedObjectVariableName(node);

  return objectName ? [...breadcrumb, objectName] : breadcrumb;
}

export function extractJavascriptTypescriptSignature(
  node: TSNode,
): string | undefined {
  if (node.type === "pair") {
    const key = extractJavascriptTypescriptName(node);
    const value = node.childForFieldName("value");
    const valueSignature = value ? extractGenericSignature(value) : undefined;

    return key && valueSignature
      ? `${key}: ${valueSignature}`
      : extractGenericSignature(node);
  }

  return extractGenericSignature(node);
}

export function classifyJavascriptTypescriptNode(
  node: TSNode,
): CodeSymbolType | undefined {
  if (
    node.type === "pair" ||
    JS_TS_FUNCTION_VALUE_DECLARATION_TYPES.has(node.type)
  ) {
    return hasFunctionValue(node) ? "function" : undefined;
  }

  return undefined;
}

export const extractJavascriptTypescriptDoc = extractPrecedingDoc;

export const extractJavascriptTypescriptModifiers = extractCommonModifiers;

function hasFunctionValue(node: TSNode): boolean {
  const value =
    node.childForFieldName("value") ??
    node.namedChildren.find((child) =>
      JS_TS_FUNCTION_VALUE_TYPES.has(child.type),
    );

  return value !== undefined && containsFunctionValue(value);
}

function containsFunctionValue(node: TSNode): boolean {
  if (JS_TS_FUNCTION_VALUE_TYPES.has(node.type)) {
    return true;
  }

  if (node.type !== "call_expression" && node.type !== "arguments") {
    return false;
  }

  return node.namedChildren.some((child) => containsFunctionValue(child));
}

function exportedObjectFunctionEntities(node: TSNode): TSNode[] {
  if (!isExportedVariableDeclarator(node)) {
    return [];
  }

  const value = node.childForFieldName("value");
  if (
    !value ||
    (value.type !== "object" && value.type !== "object_expression")
  ) {
    return [];
  }

  return value.namedChildren.filter(
    (child) =>
      (child.type === "pair" && hasFunctionValue(child)) ||
      child.type === "method_definition",
  );
}

function exportedObjectVariableName(node: TSNode): string | undefined {
  const object =
    closestAncestor(node, "object") ??
    closestAncestor(node, "object_expression");
  const variable = object?.parent;

  if (
    !variable ||
    variable.type !== "variable_declarator" ||
    !isExportedVariableDeclarator(variable)
  ) {
    return undefined;
  }

  return extractJavascriptTypescriptName(variable);
}

function isExportedVariableDeclarator(node: TSNode): boolean {
  if (node.type !== "variable_declarator") {
    return false;
  }

  return closestAncestor(node, "export_statement") !== undefined;
}

function isObjectMember(node: TSNode): boolean {
  return (
    node.parent?.type === "object" || node.parent?.type === "object_expression"
  );
}

function findNamedIdentifierChild(node: TSNode): TSNode | undefined {
  return node.namedChildren.find(
    (child) =>
      child.type === "identifier" ||
      child.type === "property_identifier" ||
      child.type === "type_identifier",
  );
}
