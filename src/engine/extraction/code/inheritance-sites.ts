import type { TSNode } from "./tree-sitter/nodes.js";
import {
  referenceTargetFromSyntax,
  type ReferenceTarget,
} from "../../reference-target.js";

export type InheritanceSite = {
  /** Base / interface name as written (may be qualified). */
  name: string;
  target: ReferenceTarget;
  /** 1-based source line of the heritage clause entry. */
  line: number;
  kind: "extends" | "implements";
};

type RawInheritanceSite = Omit<InheritanceSite, "target">;

const MAX_TYPE_NAME_CHARS = 180;

/**
 * Collect extends/implements (and language equivalents) from a type entity node.
 * Does not resolve — graph layer builds local INHERITS or pending Refs.
 */
export function collectInheritanceSites(
  node: TSNode,
  language: string,
): InheritanceSite[] {
  const sites: RawInheritanceSite[] = (() => {
    switch (language) {
      case "typescript":
      case "tsx":
      case "javascript":
      case "jsx":
        return collectJsTs(node);
      case "python":
        return collectPython(node);
      case "java":
        return collectJava(node);
      case "cpp":
      case "c":
        return collectCpp(node);
      case "rust":
        return collectRust(node);
      case "go":
        return collectGo(node);
      default:
        return [];
    }
  })();
  return sites.map((site) => ({
    ...site,
    target: referenceTargetFromSyntax(site.name),
  }));
}

function collectJsTs(node: TSNode): RawInheritanceSite[] {
  const sites: RawInheritanceSite[] = [];

  if (
    node.type === "class_declaration" ||
    node.type === "abstract_class_declaration"
  ) {
    const heritage = node.namedChildren.find(
      (c) => c.type === "class_heritage",
    );
    if (!heritage) {
      return sites;
    }
    for (const clause of heritage.namedChildren) {
      if (clause.type === "extends_clause") {
        pushTypeChildren(sites, clause, "extends", /*skipKeyword*/ true);
      } else if (clause.type === "implements_clause") {
        pushTypeChildren(sites, clause, "implements", true);
      }
    }
    if (sites.length === 0) {
      // tree-sitter-javascript represents `extends Base` as a direct
      // identifier inside class_heritage rather than an extends_clause.
      pushTypeChildren(sites, heritage, "extends", true);
    }
    return sites;
  }

  if (node.type === "interface_declaration") {
    const clause = node.namedChildren.find(
      (c) => c.type === "extends_type_clause" || c.type === "extends_clause",
    );
    if (clause) {
      pushTypeChildren(sites, clause, "extends", true);
    }
  }

  return sites;
}

function collectPython(node: TSNode): RawInheritanceSite[] {
  if (node.type === "decorated_definition") {
    const inner = node.namedChildren.find((c) => c.type === "class_definition");
    return inner ? collectPython(inner) : [];
  }
  if (node.type !== "class_definition") {
    return [];
  }
  const args = node.childForFieldName("superclasses");
  if (!args) {
    return [];
  }
  const sites: RawInheritanceSite[] = [];
  for (const child of args.namedChildren) {
    const name = normalizeTypeName(child);
    if (!name || isLanguageBuiltin("python", name)) {
      continue;
    }
    sites.push({
      name,
      line: child.startPosition.row + 1,
      kind: "extends",
    });
  }
  return sites;
}

function collectJava(node: TSNode): RawInheritanceSite[] {
  const sites: RawInheritanceSite[] = [];
  if (node.type === "class_declaration" || node.type === "enum_declaration") {
    const superclass =
      node.childForFieldName("superclass") ??
      node.namedChildren.find((c) => c.type === "superclass");
    if (superclass) {
      pushTypeChildren(sites, superclass, "extends", true);
    }
    const ifaces =
      node.childForFieldName("interfaces") ??
      node.namedChildren.find((c) => c.type === "super_interfaces");
    if (ifaces) {
      const list =
        ifaces.namedChildren.find((c) => c.type === "type_list") ?? ifaces;
      pushTypeChildren(sites, list, "implements", true);
    }
    return sites;
  }
  if (node.type === "interface_declaration") {
    const ifaces =
      node.childForFieldName("interfaces") ??
      node.namedChildren.find(
        (c) => c.type === "extends_interfaces" || c.type === "extends_clause",
      );
    if (ifaces) {
      const list =
        ifaces.namedChildren.find((c) => c.type === "type_list") ?? ifaces;
      pushTypeChildren(sites, list, "extends", true);
    }
  }
  return sites;
}

function collectCpp(node: TSNode): RawInheritanceSite[] {
  if (node.type !== "class_specifier" && node.type !== "struct_specifier") {
    return [];
  }
  const clause = node.namedChildren.find((c) => c.type === "base_class_clause");
  if (!clause) {
    return [];
  }
  const sites: RawInheritanceSite[] = [];
  for (const child of clause.namedChildren) {
    if (
      child.type === "access_specifier" ||
      child.type === "attribute_declaration"
    ) {
      continue;
    }
    const name = normalizeTypeName(child);
    if (!name) {
      continue;
    }
    sites.push({
      name,
      line: child.startPosition.row + 1,
      kind: "extends",
    });
  }
  return sites;
}

function collectRust(node: TSNode): RawInheritanceSite[] {
  if (node.type !== "impl_item") {
    return [];
  }
  const trait = node.childForFieldName("trait");
  if (!trait) {
    return [];
  }
  const name = normalizeTypeName(trait);
  if (!name) {
    return [];
  }
  return [
    {
      name,
      line: trait.startPosition.row + 1,
      kind: "implements",
    },
  ];
}

function collectGo(node: TSNode): RawInheritanceSite[] {
  if (node.type !== "type_spec") {
    return [];
  }
  const typeNode = node.childForFieldName("type");
  if (!typeNode) {
    return [];
  }
  const sites: RawInheritanceSite[] = [];

  if (typeNode.type === "struct_type") {
    const fields = typeNode.namedChildren.find(
      (c) => c.type === "field_declaration_list",
    );
    for (const field of fields?.namedChildren ?? []) {
      if (field.type !== "field_declaration") {
        continue;
      }
      // Embedded field: no field name, only a type.
      if (field.childForFieldName("name") || hasFieldIdentifier(field)) {
        continue;
      }
      const typeChild =
        field.childForFieldName("type") ??
        field.namedChildren.find((c) => c.type !== "field_identifier");
      const name = typeChild ? normalizeTypeName(typeChild) : undefined;
      if (!name || isLanguageBuiltin("go", name)) {
        continue;
      }
      sites.push({
        name,
        line: field.startPosition.row + 1,
        kind: "extends",
      });
    }
    return sites;
  }

  if (typeNode.type === "interface_type") {
    for (const child of typeNode.namedChildren) {
      if (child.type !== "constraint_elem" && child.type !== "type_elem") {
        continue;
      }
      const name = normalizeTypeName(child);
      if (!name || isLanguageBuiltin("go", name)) {
        continue;
      }
      sites.push({
        name,
        line: child.startPosition.row + 1,
        kind: "extends",
      });
    }
  }

  return sites;
}

function pushTypeChildren(
  sites: RawInheritanceSite[],
  clause: TSNode,
  kind: "extends" | "implements",
  skipKeyword: boolean,
): void {
  for (const child of clause.namedChildren) {
    if (
      skipKeyword &&
      (child.type === "extends" || child.type === "implements")
    ) {
      continue;
    }
    if (
      child.type === "extends" ||
      child.type === "implements" ||
      child.type === "," ||
      child.type === "comment"
    ) {
      continue;
    }
    // Nested type_list already expanded by caller when needed; still accept
    // direct type nodes and type_list children.
    if (child.type === "type_list") {
      pushTypeChildren(sites, child, kind, false);
      continue;
    }
    const name = normalizeTypeName(child);
    if (!name) {
      continue;
    }
    sites.push({
      name,
      line: child.startPosition.row + 1,
      kind,
    });
  }
}

function hasFieldIdentifier(field: TSNode): boolean {
  return field.namedChildren.some((c) => c.type === "field_identifier");
}

function normalizeTypeName(node: TSNode): string | undefined {
  let current: TSNode | null = node;

  while (current) {
    if (
      current.type === "parenthesized_expression" ||
      current.type === "parenthesized_type" ||
      current.type === "constraint_elem" ||
      current.type === "type_elem"
    ) {
      current = current.namedChildren[0] ?? null;
      continue;
    }
    if (
      current.type === "pointer_type" ||
      current.type === "reference_type" ||
      current.type === "qualified_type" ||
      current.type === "scoped_type_identifier"
    ) {
      // Prefer the innermost named type identifier.
      const named: TSNode | null | undefined =
        current.childForFieldName("type") ??
        current.childForFieldName("name") ??
        current.namedChildren[current.namedChildren.length - 1];
      current = named ?? null;
      continue;
    }
    if (current.type === "generic_type" || current.type === "template_type") {
      current =
        current.childForFieldName("name") ?? current.namedChildren[0] ?? null;
      continue;
    }
    break;
  }

  if (!current) {
    return undefined;
  }

  let text = current.text
    .replace(/\s+/g, " ")
    .replace(/<[\s\S]*>$/, "")
    .replace(/^\*+\s*/, "")
    .replace(/^&\s*/, "")
    .trim();

  // Drop trailing type arguments if nested whitespace broke the regex.
  const angle = text.indexOf("<");
  if (angle > 0) {
    text = text.slice(0, angle).trim();
  }

  if (
    text.length === 0 ||
    text.length > MAX_TYPE_NAME_CHARS ||
    /[\n\r]/.test(text) ||
    !/[A-Za-z_][A-Za-z0-9_$.]*/.test(text)
  ) {
    return undefined;
  }

  return text;
}

const PYTHON_BASE_BUILTINS = new Set([
  "object",
  "Exception",
  "BaseException",
  "type",
  "int",
  "str",
  "list",
  "dict",
  "tuple",
  "set",
  "bool",
  "float",
  "bytes",
]);

const GO_BASE_BUILTINS = new Set([
  "any",
  "bool",
  "byte",
  "complex64",
  "complex128",
  "error",
  "float32",
  "float64",
  "int",
  "int8",
  "int16",
  "int32",
  "int64",
  "rune",
  "string",
  "uint",
  "uint8",
  "uint16",
  "uint32",
  "uint64",
  "uintptr",
]);

function isLanguageBuiltin(language: string, name: string): boolean {
  const bare = name.split(".").pop() ?? name;
  if (language === "python") {
    return PYTHON_BASE_BUILTINS.has(bare) || PYTHON_BASE_BUILTINS.has(name);
  }
  if (language === "go") {
    return GO_BASE_BUILTINS.has(bare) || GO_BASE_BUILTINS.has(name);
  }
  return false;
}
