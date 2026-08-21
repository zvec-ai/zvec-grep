import type { TSNode } from "./tree-sitter/nodes.js";

const PARAMETER_LIST_TYPES = new Set([
  "parameters",
  "formal_parameters",
  "parameter_list",
]);

/** Extract callable arity from the language AST instead of signature text. */
export function extractCallableArity(
  node: TSNode,
  language: string,
): number | undefined {
  const parameters = findParameterList(node);
  if (!parameters) return undefined;
  const entries = parameters.namedChildren.filter(
    (child) =>
      !child.type.includes("comment") && child.type !== "type_parameters",
  );
  if (entries.length === 1 && /^(?:void)?$/.test(entries[0]!.text.trim()))
    return 0;
  return entries.reduce(
    (count, parameter, index) =>
      count + parameterArity(parameter, language, index),
    0,
  );
}

function findParameterList(node: TSNode): TSNode | undefined {
  const direct = node.childForFieldName("parameters");
  if (direct) return direct;
  const body = node.childForFieldName("body");
  const queue = [...(node.namedChildren ?? [])];
  for (let depth = 0; queue.length > 0 && depth < 64; depth++) {
    const current = queue.shift()!;
    if (body && sameSyntaxNode(current, body)) continue;
    if (PARAMETER_LIST_TYPES.has(current.type)) return current;
    queue.push(...(current.namedChildren ?? []));
  }
  return undefined;
}

function parameterArity(node: TSNode, language: string, index: number): number {
  const text = node.text.trim();
  if (
    language === "rust" &&
    (node.type === "self_parameter" || /^(?:&\s*)?(?:mut\s+)?self\b/.test(text))
  )
    return 0;
  if (language === "python" && index === 0 && /^(?:self|cls)\b/.test(text))
    return 0;
  if (language === "go") {
    const typeNode = node.childForFieldName("type");
    const names = node.namedChildren.filter(
      (child) =>
        (!typeNode || !sameSyntaxNode(child, typeNode)) &&
        /identifier$/.test(child.type),
    );
    if (names.length > 0) return names.length;
  }
  return 1;
}

function sameSyntaxNode(left: TSNode, right: TSNode): boolean {
  return (
    left.type === right.type &&
    left.startIndex === right.startIndex &&
    left.endIndex === right.endIndex
  );
}
