import Parser from "web-tree-sitter";
import { ensureParser, loadGrammar } from "./grammar.js";

export async function withParser<T>(
  source: string,
  format: string,
  fn: (tree: Parser.Tree) => T | Promise<T>,
): Promise<T | null> {
  await ensureParser();

  const grammar = await loadGrammar(format);
  if (!grammar) {
    return null;
  }

  const parser = new Parser();
  parser.setLanguage(grammar);

  let tree: Parser.Tree | null;

  try {
    tree = parser.parse(source);
  } catch {
    parser.delete();
    return null;
  }

  if (!tree) {
    parser.delete();
    return null;
  }

  try {
    return await fn(tree);
  } finally {
    tree.delete();
    parser.delete();
  }
}
