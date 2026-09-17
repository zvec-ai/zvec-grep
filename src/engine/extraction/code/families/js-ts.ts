import type { CodeSymbolType } from "../../../types.js";
import type { TSNode } from "../tree-sitter/nodes.js";
import {
  closestAncestor,
  extractCommonModifiers,
  extractGenericSignature,
  extractPrecedingDoc,
} from "./metadata.js";

const JS_TS_FUNCTION_VALUE_DECLARATION_TYPES = new Set([
  "assignment_expression",
  "field_definition",
  "public_field_definition",
  "variable_declarator",
]);

const JS_TS_FUNCTION_VALUE_TYPES = new Set([
  "arrow_function",
  "function_expression",
  "generator_function",
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

/** Keep declaration prefixes with a single symbol, not as separate fragments. */
export function javascriptTypescriptSourceNode(node: TSNode): TSNode {
  let current = node;
  while (current.parent) {
    const parent = current.parent;
    if (
      ![
        "export_statement",
        "lexical_declaration",
        "variable_declaration",
        "expression_statement",
      ].includes(parent.type)
    )
      break;
    const declarations = parent.namedChildren.filter(
      (child) => child.type !== "comment",
    );
    if (declarations.length !== 1 || declarations[0]!.id !== current.id) break;
    current = parent;
  }
  return current;
}

export function extractJavascriptTypescriptName(
  node: TSNode,
): string | undefined {
  if (node.type === "assignment_expression")
    return assignmentBinding(node)?.name;
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
  const receiver = assignmentBinding(node)?.scope;
  if (receiver) return [...breadcrumb, receiver];
  const objectName = exportedObjectVariableName(node);

  return objectName ? [...breadcrumb, objectName] : breadcrumb;
}

export function extractJavascriptTypescriptSignature(
  node: TSNode,
): string | undefined {
  const assigned = assignedFunction(node);
  if (assigned) {
    const signature = extractGenericSignature(assigned);
    const target = node.childForFieldName("left")?.text;
    if (signature && target) return `${target} = ${signature}`;
  }
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

export function extractJavascriptTypescriptDoc(
  node: TSNode,
): string | undefined {
  return extractPrecedingDoc(
    node.type === "assignment_expression" &&
      node.parent?.type === "expression_statement"
      ? node.parent
      : node,
  );
}

export function extractJavascriptTypescriptModifiers(node: TSNode) {
  return extractCommonModifiers(assignedFunction(node) ?? node);
}

function hasFunctionValue(node: TSNode): boolean {
  if (node.type === "assignment_expression") {
    return (
      assignmentBinding(node) !== undefined &&
      assignedFunction(node) !== undefined
    );
  }
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

  if (
    node.type !== "call_expression" &&
    node.type !== "arguments" &&
    node.type !== "parenthesized_expression"
  ) {
    return false;
  }

  return node.namedChildren.some((child) => containsFunctionValue(child));
}

function assignedFunction(node: TSNode): TSNode | undefined {
  if (node.type !== "assignment_expression") return undefined;
  let value = node.childForFieldName("right");
  while (value?.type === "parenthesized_expression")
    value = value.namedChildren[0] ?? null;
  // A call that accepts a callback may return an array, subscription, etc.
  // Do not infer a function-valued binding merely from a nested callback.
  return value && JS_TS_FUNCTION_VALUE_TYPES.has(value.type)
    ? value
    : undefined;
}

function assignmentBinding(
  node: TSNode,
): { name: string; scope?: string } | undefined {
  if (node.type !== "assignment_expression") return undefined;
  const target = node.childForFieldName("left");
  if (target?.type === "identifier") return { name: target.text };
  if (target?.type === "member_expression") {
    const property = target.childForFieldName("property");
    const object = target.childForFieldName("object");
    if (property && object) return { name: property.text, scope: object.text };
  }
  if (target?.type === "subscript_expression") {
    const index = target.childForFieldName("index");
    const object = target.childForFieldName("object");
    // Only a known literal name is a lookup anchor. Never claim that the
    // variable in object[name], or undecoded escape text, defines that name.
    if (
      index?.type === "string" &&
      object &&
      index.namedChildren.every((child) => child.type === "string_fragment")
    ) {
      const name = index.text.slice(1, -1);
      if (name) return { name, scope: object.text };
    }
  }
  return undefined;
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
