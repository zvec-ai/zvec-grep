import type Parser from "web-tree-sitter";

export type TSNode = Parser.SyntaxNode;

export function findIdentifierLeaf(node: TSNode): TSNode | null {
  const wrappers = new Set([
    "array_declarator",
    "function_declarator",
    "init_declarator",
    "parenthesized_declarator",
    "pointer_declarator",
    "reference_declarator",
  ]);

  let current: TSNode | null = node;

  for (let depth = 0; current && depth < 16; depth++) {
    if (current.type === "identifier" || current.type.endsWith("_identifier")) {
      return current;
    }

    if (
      current.type === "destructor_name" ||
      current.type === "operator_name"
    ) {
      return current;
    }

    if (wrappers.has(current.type)) {
      current = current.childForFieldName("declarator");
      continue;
    }

    return null;
  }

  return null;
}
