import type { LanguageAdapter } from "./adapter.js";
import {
  referenceTargetFromSyntax,
  memberReferenceTarget,
  type ReferenceTarget,
} from "../../reference-target.js";
import { isCallNode } from "./call-sites.js";
import type { TSNode } from "./tree-sitter/nodes.js";

export type RefSite = {
  /** Referenced name as written (may be qualified). */
  name: string;
  target: ReferenceTarget;
  /** 1-based source line. */
  line: number;
  /** REFS rel: type | return | member | decorates */
  kind: "type" | "return" | "member" | "decorates";
};

const MAX_REF_NAME_CHARS = 180;

const TS_PREDEFINED = new Set([
  "string",
  "number",
  "boolean",
  "void",
  "any",
  "never",
  "unknown",
  "null",
  "undefined",
  "object",
  "bigint",
  "symbol",
  "this",
  "true",
  "false",
]);

const PYTHON_PREDEFINED = new Set([
  "str",
  "int",
  "float",
  "bool",
  "bytes",
  "list",
  "dict",
  "tuple",
  "set",
  "None",
  "Any",
  "Optional",
  "Union",
  "List",
  "Dict",
  "Tuple",
  "Set",
  "Callable",
  "Type",
  "type",
  "object",
  "self",
  "cls",
]);

const HERITAGE_CONTEXTS = new Set([
  "class_heritage",
  "extends_clause",
  "implements_clause",
  "extends_type_clause",
  "superclass",
  "super_interfaces",
  "extends_interfaces",
  "base_class_clause",
]);

/**
 * Walk an entity node for type / member / decorator references.
 * Skips nested indexed entities and call callees (owned by CALLS).
 */
export function collectRefSites(
  node: TSNode,
  adapter?: LanguageAdapter | null,
  language = "typescript",
): RefSite[] {
  const sites: RefSite[] = [];
  const entityTypes = adapter?.entityTypes;
  const seen = new Set<string>();

  const push = (
    site: Omit<RefSite, "target"> & { target?: ReferenceTarget },
  ): void => {
    if (!site.name || isNoiseName(site.name, language)) {
      return;
    }
    const key = `${site.kind}\0${site.name}\0${site.line}`;
    if (seen.has(key)) {
      return;
    }
    seen.add(key);
    sites.push({
      ...site,
      target: site.target ?? referenceTargetFromSyntax(site.name),
    });
  };

  const visit = (current: TSNode | null, skipSelfEntity: boolean): void => {
    if (!current) {
      return;
    }

    if (
      !skipSelfEntity &&
      entityTypes?.has(current.type) &&
      adapter?.shouldIndexEntity?.(current) !== false
    ) {
      return;
    }

    // Decorators attached to this entity (or nested non-entity nodes).
    if (current.type === "decorator") {
      const name = decoratorName(current);
      if (name) {
        push({
          name,
          line: current.startPosition.row + 1,
          kind: "decorates",
        });
      }
      // Still walk children for nested type args.
    }

    if (current.type === "type_annotation" || current.type === "type") {
      const kind = isReturnTypeAnnotation(current) ? "return" : "type";
      for (const name of typeNamesIn(current, language)) {
        if (!inHeritageContext(current)) {
          push({ name, line: current.startPosition.row + 1, kind });
        }
      }
      // Children walked below for nested structures.
    }

    // Member / property access that is not a call callee.
    if (
      (current.type === "member_expression" ||
        current.type === "attribute" ||
        current.type === "field_expression" ||
        current.type === "field_access") &&
      !isCallCallee(current)
    ) {
      const target = memberTarget(current);
      if (target) {
        push({
          name: target.raw,
          target,
          line: current.startPosition.row + 1,
          kind: "member",
        });
      }
    }

    for (const child of current.namedChildren ?? []) {
      visit(child, false);
    }
  };

  visit(node, true);

  // Decorators may sit on a parent export_statement / decorated_definition.
  const parent = node.parent;
  if (
    parent?.type === "export_statement" ||
    parent?.type === "decorated_definition"
  ) {
    for (const child of parent.namedChildren ?? []) {
      if (child.type === "decorator") {
        const name = decoratorName(child);
        if (name) {
          push({
            name,
            line: child.startPosition.row + 1,
            kind: "decorates",
          });
        }
      }
    }
  }

  return sites;
}

function isReturnTypeAnnotation(node: TSNode): boolean {
  const parent = node.parent;
  if (!parent) {
    return false;
  }
  // TS/JS: function_declaration / method_definition / arrow_function → type_annotation after params
  if (
    parent.type.includes("function") ||
    parent.type === "method_definition" ||
    parent.type === "method_signature" ||
    parent.type === "arrow_function" ||
    parent.type === "function_declaration" ||
    parent.type === "generator_function_declaration" ||
    parent.type === "function_definition"
  ) {
    const params =
      parent.childForFieldName("parameters") ??
      parent.namedChildren.find(
        (c) =>
          c.type === "formal_parameters" ||
          c.type === "parameters" ||
          c.type === "parameter_list",
      );
    if (params && node.startIndex >= params.endIndex) {
      return true;
    }
    // Python: field name "return_type" or sibling type after parameters
    if (parent.childForFieldName("return_type") === node) {
      return true;
    }
    if (
      parent.type === "function_definition" &&
      node.type === "type" &&
      parent.childForFieldName("parameters") &&
      node.startIndex > (parent.childForFieldName("parameters")?.endIndex ?? 0)
    ) {
      return true;
    }
  }
  return false;
}

function typeNamesIn(node: TSNode, language: string): string[] {
  const out: string[] = [];
  const visit = (current: TSNode | null): void => {
    if (!current) {
      return;
    }
    if (
      current.type === "predefined_type" ||
      current.type === "literal_type" ||
      current.type === "null" ||
      current.type === "undefined"
    ) {
      return;
    }
    if (
      current.type === "type_identifier" ||
      current.type === "identifier" ||
      current.type === "nested_type_identifier" ||
      current.type === "member_expression" ||
      current.type === "scoped_type_identifier" ||
      current.type === "qualified_type"
    ) {
      // Avoid treating parameter names as types: only under type-ish parents.
      if (
        current.type === "identifier" &&
        current.parent &&
        !isTypeishParent(current.parent)
      ) {
        // keep walking
      } else {
        const name = normalizeRefName(current.text);
        if (name && !isNoiseName(name, language)) {
          out.push(name);
          return; // don't also collect nested pieces of qualified name
        }
      }
    }
    if (current.type === "generic_type" || current.type === "template_type") {
      const nameNode =
        current.childForFieldName("name") ?? current.namedChildren[0];
      const name = nameNode ? normalizeRefName(nameNode.text) : undefined;
      if (name && !isNoiseName(name, language)) {
        out.push(name);
      }
      // Still collect type arguments.
      for (const child of current.namedChildren ?? []) {
        if (child === nameNode) {
          continue;
        }
        visit(child);
      }
      return;
    }
    for (const child of current.namedChildren ?? []) {
      visit(child);
    }
  };
  visit(node);
  return out;
}

function isTypeishParent(parent: TSNode): boolean {
  return (
    parent.type === "type" ||
    parent.type === "type_annotation" ||
    parent.type === "generic_type" ||
    parent.type === "union_type" ||
    parent.type === "intersection_type" ||
    parent.type === "type_arguments" ||
    parent.type.includes("type")
  );
}

function decoratorName(node: TSNode): string | undefined {
  const call = node.namedChildren.find((c) => c.type === "call_expression");
  if (call) {
    const fn = call.childForFieldName("function") ?? call.namedChildren[0];
    return fn ? normalizeRefName(fn.text) : undefined;
  }
  const id =
    node.namedChildren.find(
      (c) =>
        c.type === "identifier" ||
        c.type === "member_expression" ||
        c.type === "decorator_member_expression",
    ) ?? node.namedChildren[0];
  if (!id) {
    return undefined;
  }
  return normalizeRefName(id.text.replace(/^@/, ""));
}

function memberTarget(node: TSNode): ReferenceTarget | undefined {
  const prop =
    node.childForFieldName("property") ??
    node.childForFieldName("field") ??
    node.namedChildren[node.namedChildren.length - 1];
  if (!prop) {
    return undefined;
  }
  if (
    prop.type !== "property_identifier" &&
    prop.type !== "identifier" &&
    prop.type !== "field_identifier" &&
    prop.type !== "property"
  ) {
    return undefined;
  }
  const member = normalizeRefName(prop.text);
  const receiver =
    node.childForFieldName("object") ??
    node.childForFieldName("argument") ??
    node.childForFieldName("value") ??
    node.namedChildren[0];
  if (!member || !receiver || receiver === prop) return undefined;
  return memberReferenceTarget(node.text, receiver.text, member);
}

function isCallCallee(node: TSNode): boolean {
  const parent = node.parent;
  if (!parent || !isCallNode(parent)) {
    return false;
  }
  const fn =
    parent.childForFieldName("function") ??
    parent.childForFieldName("name") ??
    parent.namedChildren[0];
  return sameNodeRange(fn, node);
}

/** Tree-sitter may return a fresh JS wrapper for the same syntax node. */
function sameNodeRange(left: TSNode | undefined, right: TSNode): boolean {
  return (
    left !== undefined &&
    left.type === right.type &&
    left.startIndex === right.startIndex &&
    left.endIndex === right.endIndex
  );
}

function inHeritageContext(node: TSNode): boolean {
  let current: TSNode | null = node;
  for (let i = 0; i < 8 && current; i++) {
    if (HERITAGE_CONTEXTS.has(current.type)) {
      return true;
    }
    current = current.parent;
  }
  return false;
}

function normalizeRefName(value: string): string | undefined {
  let text = value
    .replace(/\s+/g, " ")
    .replace(/^@/, "")
    .replace(/<[\s\S]*>$/, "")
    .trim();
  const angle = text.indexOf("<");
  if (angle > 0) {
    text = text.slice(0, angle).trim();
  }
  if (
    text.length === 0 ||
    text.length > MAX_REF_NAME_CHARS ||
    /[\n\r]/.test(text) ||
    !/^[A-Za-z_$][A-Za-z0-9_$.]*$/.test(text)
  ) {
    return undefined;
  }
  return text;
}

function isNoiseName(name: string, language: string): boolean {
  const bare = name.split(".").pop() ?? name;
  if (bare.length <= 1 && bare !== "_") {
    // keep single-letter type params? drop them — too noisy
    return bare.length <= 1;
  }
  if (TS_PREDEFINED.has(bare) || TS_PREDEFINED.has(name)) {
    return true;
  }
  if (
    (language === "python" || language === "py") &&
    (PYTHON_PREDEFINED.has(bare) || PYTHON_PREDEFINED.has(name))
  ) {
    return true;
  }
  return false;
}
