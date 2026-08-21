import type { CodeEntityModifier } from "../../../types.js";
import type { TSNode } from "../tree-sitter/nodes.js";

const COMMENT_TYPES = new Set([
  "comment",
  "line_comment",
  "block_comment",
  "documentation_comment",
]);

export function extractGenericSignature(node: TSNode): string | undefined {
  const body =
    node.childForFieldName("body") ??
    node.namedChildren.find(
      (child) =>
        child.type === "statement_block" ||
        child.type === "compound_statement" ||
        child.type === "block" ||
        child.type === "class_body" ||
        child.type === "declaration_list" ||
        child.type === "field_declaration_list",
    );

  const text =
    body && body.startIndex > node.startIndex
      ? node.text.slice(0, body.startIndex - node.startIndex).trimEnd()
      : firstNonEmptyLine(node.text);

  const normalized = text
    .replace(/\s+/g, " ")
    .replace(/\s*[{;]\s*$/, "")
    .trim();

  return normalized.length > 0 ? normalized : undefined;
}

export function extractPrecedingDoc(node: TSNode): string | undefined {
  const comments: string[] = [];
  let sibling = node.previousNamedSibling;

  while (sibling && COMMENT_TYPES.has(sibling.type)) {
    comments.unshift(cleanCommentText(sibling.text));
    sibling = sibling.previousNamedSibling;
  }

  const doc = comments.join("\n").trim();

  return doc.length > 0 ? doc : undefined;
}

export function extractCommonModifiers(node: TSNode): CodeEntityModifier[] {
  const modifiers = new Set<CodeEntityModifier>();
  const signature =
    extractGenericSignature(node) ?? firstNonEmptyLine(node.text);

  if (hasAncestor(node, "export_statement")) {
    modifiers.add("exported");
  }

  for (const token of signature.matchAll(
    /\b(public|private|protected|internal|static|abstract|async|pub)\b/g,
  )) {
    modifiers.add(normalizeModifier(token[1]));
  }

  return [...modifiers];
}

export function isInsideNodeType(node: TSNode, type: string): boolean {
  let parent = node.parent;

  while (parent) {
    if (parent.type === type) {
      return true;
    }
    parent = parent.parent;
  }

  return false;
}

export function closestAncestor(
  node: TSNode,
  type: string,
): TSNode | undefined {
  let parent = node.parent;

  while (parent) {
    if (parent.type === type) {
      return parent;
    }
    parent = parent.parent;
  }

  return undefined;
}

export function firstNonEmptyLine(text: string): string {
  return (
    text
      .split(/\r?\n/)
      .find((line) => line.trim().length > 0)
      ?.trim() ?? ""
  );
}

function hasAncestor(node: TSNode, type: string): boolean {
  return closestAncestor(node, type) !== undefined;
}

function normalizeModifier(value: string): CodeEntityModifier {
  return value === "pub" ? "public" : (value as CodeEntityModifier);
}

function cleanCommentText(text: string): string {
  return text
    .replace(/^\/\*\*?/, "")
    .replace(/\*\/$/, "")
    .replace(/^\/\/\/?\s?/gm, "")
    .replace(/^#\s?/gm, "")
    .replace(/^\s*\*\s?/gm, "")
    .trim();
}
