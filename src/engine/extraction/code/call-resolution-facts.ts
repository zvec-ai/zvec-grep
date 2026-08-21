import type { ReferenceTarget } from "../../reference-target.js";
import type { LanguageAdapter } from "./adapter.js";
import type { TSNode } from "./tree-sitter/nodes.js";

export type CallResolutionFact = {
  receiverTypes: ReadonlyMap<string, string>;
  ownerFieldTypes: ReadonlyMap<string, string>;
  dynamicReceivers: ReadonlyMap<string, readonly string[]>;
  genericBounds: ReadonlyMap<string, readonly string[]>;
  language: string;
};

/** Build the type environment visible at each call site in source order. */
export function extractCallResolutionFacts(
  node: TSNode,
  adapter: LanguageAdapter,
): ReadonlyMap<string, CallResolutionFact> {
  const facts = new Map<string, CallResolutionFact>();
  const scopes: Map<string, string>[] = [initialBindings(node, adapter)];
  const ownerFieldTypes = collectOwnerFields(node, adapter.format);
  const dynamicReceivers = collectDynamicReceivers(node, adapter);
  const genericBounds = collectGenericBounds(node, adapter.format);

  const visit = (current: TSNode | null, skipSelfEntity: boolean): void => {
    if (!current) return;
    if (
      !skipSelfEntity &&
      adapter.entityTypes.has(current.type) &&
      adapter.shouldIndexEntity?.(current) !== false
    )
      return;

    const opensScope =
      !skipSelfEntity && opensLexicalScope(current.type, adapter.format);
    if (opensScope) scopes.push(new Map());

    if (DECLARATION_TYPES.has(current.type)) {
      // Initializers see the environment before their binding is introduced.
      for (const child of current.namedChildren ?? []) visit(child, false);
      const scope = scopes.at(-1)!;
      for (const binding of declarationBindings(current, adapter.format))
        scope.set(binding.name, binding.type);
      if (opensScope) scopes.pop();
      return;
    }

    if (isCallNodeType(current.type)) {
      facts.set(callNodeKey(current), {
        receiverTypes: flattenScopes(scopes),
        ownerFieldTypes,
        dynamicReceivers,
        genericBounds,
        language: adapter.format,
      });
    }
    for (const child of current.namedChildren ?? []) visit(child, false);
    if (opensScope) scopes.pop();
  };

  visit(node, true);
  return facts;
}

export function enrichTargetWithResolutionFact(
  target: ReferenceTarget,
  fact: CallResolutionFact | undefined,
  arity: number | undefined,
): ReferenceTarget {
  const arityHints = arity === undefined ? {} : { callArity: arity };
  const receiver = target.receiver?.name;
  if (!receiver)
    return Object.keys(arityHints).length === 0
      ? target
      : { ...target, hints: { ...target.hints, ...arityHints } };
  if (!fact) return target;
  const receiverTail = receiver.split(".").pop() ?? receiver;
  const dynamicTypes =
    fact.dynamicReceivers.get(receiver) ??
    fact.dynamicReceivers.get(receiverTail);
  const receiverType =
    dynamicTypes?.[0] ??
    (isOwnerFieldReceiver(receiver)
      ? fact.ownerFieldTypes.get(receiverTail)
      : (fact.receiverTypes.get(receiver) ??
        fact.receiverTypes.get(receiverTail)));
  if (!receiverType) return target;
  const bounds = fact.genericBounds.get(receiverType);
  const dynamicDispatch = Boolean(dynamicTypes?.length);
  return {
    ...target,
    hints: {
      ...target.hints,
      receiverType,
      ...arityHints,
      ...(bounds ? { genericBounds: [...bounds] } : {}),
      candidateTypes: dynamicTypes?.length
        ? [...dynamicTypes]
        : bounds
          ? [receiverType, ...bounds]
          : [receiverType],
      ...(bounds && bounds.length > 0
        ? { dispatch: dispatchForLanguage(fact.language) }
        : dynamicDispatch
          ? { dispatch: dispatchForLanguage(fact.language) }
          : VIRTUAL_LANGUAGES.has(fact.language)
            ? { dispatch: "virtual" as const }
            : {}),
    },
  };
}

export function callNodeKey(node: TSNode): string {
  return `${node.type}:${node.startIndex}:${node.endIndex}`;
}

const VIRTUAL_LANGUAGES = new Set(["java", "cpp", "typescript", "tsx"]);
const PARAMETER_TYPES = new Set([
  "parameter_declaration",
  "formal_parameter",
  "receiver_parameter",
  "parameter",
  "required_parameter",
  "optional_parameter",
  "variadic_parameter_declaration",
]);
const DECLARATION_TYPES = new Set([
  "variable_declarator",
  "variable_declaration",
  "local_variable_declaration",
  "lexical_declaration",
  "let_declaration",
  "field_declaration",
  "property_declaration",
  "public_field_definition",
  "short_var_declaration",
]);

function initialBindings(
  node: TSNode,
  adapter: LanguageAdapter,
): Map<string, string> {
  const bindings = new Map<string, string>();
  const parameters =
    node.childForFieldName("parameters") ??
    node.namedChildren.find((child) => /parameters/.test(child.type));
  const collect = (current: TSNode): void => {
    if (PARAMETER_TYPES.has(current.type)) {
      const binding = parameterBinding(current, adapter.format);
      if (binding) bindings.set(binding.name, binding.type);
      return;
    }
    for (const child of current.namedChildren ?? []) collect(child);
  };
  if (parameters) collect(parameters);
  else collectSignatureParameters(node, node, adapter, bindings);

  const receiver = node.childForFieldName("receiver");
  if (receiver) {
    const binding = parameterBinding(receiver, adapter.format);
    if (binding) bindings.set(binding.name, binding.type);
  }
  return bindings;
}

function collectDynamicReceivers(
  node: TSNode,
  adapter: LanguageAdapter,
): Map<string, readonly string[]> {
  const receivers = new Map<string, readonly string[]>();
  if (adapter.format !== "rust") return receivers;
  const parameters =
    node.childForFieldName("parameters") ??
    node.namedChildren.find((child) => /parameters/.test(child.type));
  const collect = (current: TSNode): void => {
    if (PARAMETER_TYPES.has(current.type)) {
      const binding = parameterBinding(current, adapter.format);
      const traits = extractDynTraits(current.text);
      if (binding && traits.length > 0) receivers.set(binding.name, traits);
      return;
    }
    for (const child of current.namedChildren ?? []) collect(child);
  };
  if (parameters) collect(parameters);
  return receivers;
}

function extractDynTraits(typeText: string): string[] {
  const traits: string[] = [];
  for (const match of typeText.matchAll(/\bdyn\s+([A-Za-z_]\w*(?:::\w+)*)/g))
    traits.push(match[1]!.split("::").pop()!);
  return [...new Set(traits)];
}

function collectSignatureParameters(
  current: TSNode,
  root: TSNode,
  adapter: LanguageAdapter,
  bindings: Map<string, string>,
): void {
  const body = root.childForFieldName("body");
  if (body && sameSyntaxNode(current, body)) return;
  if (
    !sameSyntaxNode(current, root) &&
    adapter.entityTypes.has(current.type) &&
    adapter.shouldIndexEntity?.(current) !== false
  )
    return;
  if (PARAMETER_TYPES.has(current.type)) {
    const binding = parameterBinding(current, adapter.format);
    if (binding) bindings.set(binding.name, binding.type);
    return;
  }
  for (const child of current.namedChildren ?? [])
    collectSignatureParameters(child, root, adapter, bindings);
}

function collectOwnerFields(
  node: TSNode,
  language: string,
): Map<string, string> {
  const bindings = new Map<string, string>();
  let parent = node.parent;
  for (let depth = 0; parent && depth < 3; depth++, parent = parent.parent) {
    if (!/class|struct|impl/.test(parent.type)) continue;
    const visit = (current: TSNode): void => {
      // Owner fields live outside every executable body, including the
      // current method. Entering it lets later locals overwrite a field type.
      if (/method|function|constructor/.test(current.type)) return;
      if (DECLARATION_TYPES.has(current.type)) {
        for (const binding of declarationBindings(current, language))
          bindings.set(binding.name, binding.type);
      }
      for (const child of current.namedChildren ?? []) visit(child);
    };
    visit(parent);
    break;
  }
  return bindings;
}

function isOwnerFieldReceiver(receiver: string): boolean {
  return /^(?:this|self|cls)\./.test(receiver);
}

function flattenScopes(
  scopes: readonly ReadonlyMap<string, string>[],
): Map<string, string> {
  const result = new Map<string, string>();
  for (const scope of scopes)
    for (const [name, type] of scope) result.set(name, type);
  return result;
}

function opensLexicalScope(type: string, language: string): boolean {
  if (language === "python") return false;
  return new Set([
    "statement_block",
    "compound_statement",
    "block",
    "for_statement",
    "for_in_statement",
    "enhanced_for_statement",
    "while_statement",
    "catch_clause",
  ]).has(type);
}

function declarationBindings(
  node: TSNode,
  language: string,
): { name: string; type: string }[] {
  const text = node.text.trim();
  const results: { name: string; type: string }[] = [];
  const explicit = text.match(
    /(?:^|\b(?:const|let|var)\s+)([A-Za-z_]\w*)\s*:\s*([A-Za-z_][^\s=;,)]*)/,
  );
  if (explicit)
    results.push({ name: explicit[1]!, type: normalizeType(explicit[2]!) });
  const cFamily = text.match(
    /^\s*([A-Za-z_][^\s=;,)]*)\s+([A-Za-z_]\w*)\s*(?:[=;,)])?/,
  );
  if (cFamily && !/^(?:const|let|var|return|new)$/.test(cFamily[1]!))
    results.push({ name: cFamily[2]!, type: normalizeType(cFamily[1]!) });
  const constructed = text.match(
    /([A-Za-z_]\w*)\s*(?::=|=)\s*(?:new\s+|&)?([A-Za-z_][\w.:]*)\s*(?:\{|\(|::new)/,
  );
  if (constructed)
    results.push({
      name: constructed[1]!,
      type: normalizeType(constructed[2]!),
    });
  if (language === "go") {
    const go = text.match(/^\s*var\s+([A-Za-z_]\w*)\s+([A-Za-z_][\w.]*)/);
    if (go) results.push({ name: go[1]!, type: normalizeType(go[2]!) });
  }
  return results;
}

function parameterBinding(
  node: TSNode,
  language: string,
): { name: string; type: string } | undefined {
  const name =
    node.childForFieldName("name")?.text ??
    node.childForFieldName("pattern")?.text;
  const type = node.childForFieldName("type")?.text;
  if (name && type) return { name, type: normalizeType(type) };
  const text = node.text.trim().replace(/^\(|\)$/g, "");
  const match =
    language === "go"
      ? text.match(/^([A-Za-z_]\w*)\s+\*?([A-Za-z_]\w*(?:\[[^\]]+\])?)$/)
      : language === "rust"
        ? text.match(/^(?:mut\s+)?([A-Za-z_]\w*)\s*:\s*&?(?:mut\s+)?([^=]+)$/)
        : text.match(/(?:^|\s)([A-Za-z_]\w*)\s*$/);
  if (!match) return undefined;
  if (language === "go" || language === "rust")
    return { name: match[1]!, type: normalizeType(match[2]!) };
  const inferredType = text.slice(0, text.lastIndexOf(match[1]!)).trim();
  return inferredType
    ? { name: match[1]!, type: normalizeType(inferredType) }
    : undefined;
}

function collectGenericBounds(
  node: TSNode,
  language: string,
): Map<string, readonly string[]> {
  const result = new Map<string, readonly string[]>();
  const genericTypes = new Set([
    "type_parameters",
    "type_parameter_list",
    "template_parameter_list",
  ]);
  let owner: TSNode | null = node;
  let genericNode: TSNode | undefined;
  for (let depth = 0; owner && depth < 3 && !genericNode; depth++) {
    genericNode =
      owner.childForFieldName("type_parameters") ??
      owner.namedChildren.find((child) => genericTypes.has(child.type));
    owner = owner.parent;
  }
  const text = genericNode?.text ?? "";
  const separator = language === "java" ? /\s+extends\s+/ : /\s*:\s*/;
  const inner =
    (text.startsWith("<") && text.endsWith(">")) ||
    (text.startsWith("[") && text.endsWith("]"))
      ? text.slice(1, -1)
      : text;
  for (const part of inner.split(",")) {
    const trimmed = part.trim();
    const constrained =
      language === "go"
        ? trimmed.match(/^([A-Za-z_]\w*)\s+(.+)$/)
        : language === "cpp"
          ? trimmed.match(/^([A-Za-z_]\w*)\s+([A-Za-z_]\w*)$/)
          : null;
    if (constrained && language === "go") {
      result.set(constrained[1]!, [normalizeType(constrained[2]!)]);
      continue;
    }
    if (
      constrained &&
      language === "cpp" &&
      constrained[1] !== "typename" &&
      constrained[1] !== "class"
    ) {
      result.set(constrained[2]!, [normalizeType(constrained[1]!)]);
      continue;
    }
    const pieces = trimmed.split(separator);
    const name = pieces
      .shift()
      ?.replace(/^(?:typename|class)\s+/, "")
      .trim();
    if (!name || pieces.length === 0) continue;
    const bounds = pieces
      .join(":")
      .split(/[+&]/)
      .map(normalizeType)
      .filter(Boolean);
    if (bounds.length > 0) result.set(name, bounds);
  }
  return result;
}

function dispatchForLanguage(
  language: string,
): "interface" | "trait" | "virtual" {
  if (language === "rust") return "trait";
  if (language === "go" || language === "java") return "interface";
  return "virtual";
}

function normalizeType(value: string): string {
  return (
    value
      .replace(/\b(?:const|volatile|mut|typename|class)\b/g, "")
      .replace(/[&*]/g, "")
      .replace(/<.*>|\[.*\]/g, "")
      .trim()
      .split(/\s+/)
      .pop() ?? ""
  );
}

function isCallNodeType(type: string): boolean {
  return [
    "call",
    "call_expression",
    "function_call_expression",
    "method_invocation",
    "object_creation_expression",
    "new_expression",
  ].includes(type);
}

function sameSyntaxNode(left: TSNode, right: TSNode): boolean {
  return (
    left.type === right.type &&
    left.startIndex === right.startIndex &&
    left.endIndex === right.endIndex
  );
}
